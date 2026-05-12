// jamodio-au-host — AudioUnit (AU) plugin host, macOS uniquement.
//
// C API stable consommée par src/lib.rs. Toute la complexité ObjC++ vit ici ;
// côté Rust on ne voit que des fonctions C, des handles u32 et des callbacks.
//
// Thread safety :
//   • scan / load / unload / open_editor / close_editor : main thread, sérialisés par un lock.
//   • process_stereo : audio RT thread, lock-free (pas de lock côté process — la map
//     est figée pendant que process tourne, garantie par le caller Rust).
//
// Cycle de vie ObjC : ARC actif via build.rs (-fobjc-arc). Les ivars NSObject* dans
// les structs C++ sont déclarés `__strong` explicitement.

#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <AudioToolbox/AudioToolbox.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreAudioKit/CoreAudioKit.h>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <memory>
#include <unordered_map>
#include <vector>
#include <os/lock.h>

// ---------- Types C API ----------

extern "C" {

typedef void (*au_scan_cb)(void *ctx,
                           uint32_t au_type,
                           uint32_t au_subtype,
                           uint32_t au_manuf,
                           const char *name,
                           uint32_t latency_samples,
                           int has_editor);

struct AuHostOpaque; // handle opaque pour Rust

} // extern "C"

namespace {

constexpr double kSampleRate = 48000.0;

void fourcc(uint32_t code, char out[5]) {
    out[0] = (code >> 24) & 0xFF;
    out[1] = (code >> 16) & 0xFF;
    out[2] = (code >>  8) & 0xFF;
    out[3] = (code      ) & 0xFF;
    out[4] = 0;
}

// État par plugin chargé.
//
// On utilise l'API C legacy `AudioComponentInstance` (= `AudioUnit`) plutôt
// que l'API moderne `AUAudioUnit`. Raison : `AUGenericView` (qui affiche
// l'UI générique des AU v2 = Apple natifs + AmpliTube legacy + 95% des
// plugins macOS) prend une `AudioComponentInstance` en paramètre. Pour que
// les paramètres bougés dans la GUI affectent le son, il FAUT que les deux
// (processing + GUI) parlent à la MÊME instance. `AUAudioUnit` cache son
// instance sous-jacente sans API publique → on doit donc tout faire en
// legacy. Pour les AU v3 modernes (AmpliTube récent, Battery), macOS wrappe
// transparemment la legacy AudioUnit, et `requestViewController` pourra
// être utilisé en S2 quand on raffine pour récupérer le custom UI.
struct Entry {
    AudioComponentInstance au_inst;     // instance partagée processing + GUI
    __strong NSWindow *editor_window;   // nil tant que pas ouvert
    AudioComponentDescription desc;
    std::vector<float> in_l, in_r;      // copie de l'input par bloc (callback)
    uint32_t max_frames;
    uint32_t latency_samples;
    double sample_time;                 // monotonic pour timestamp render
    // État courant lu par render_callback (set avant AudioUnitRender)
    const float *cur_in_l;
    const float *cur_in_r;
    uint32_t cur_n_frames;
};

// Callback C statique invoqué par AudioUnitRender pour fournir l'input.
// Le `refCon` pointe vers l'Entry concerné. Le callback reçoit un
// AudioBufferList que l'AU veut remplir avec les samples d'entrée.
static OSStatus jmo_render_callback(void *refCon,
                                    AudioUnitRenderActionFlags *,
                                    const AudioTimeStamp *,
                                    UInt32 /*bus*/,
                                    UInt32 frame_count,
                                    AudioBufferList *io_data) {
    Entry *e = static_cast<Entry *>(refCon);
    if (!e || !io_data || io_data->mNumberBuffers < 2) return -1;
    if (frame_count > e->cur_n_frames) return -1;
    if (!io_data->mBuffers[0].mData || !io_data->mBuffers[1].mData) {
        // Si l'AU ne nous a pas alloué de buffers, on lui passe nos pointeurs.
        io_data->mBuffers[0].mData = (void *)e->cur_in_l;
        io_data->mBuffers[0].mDataByteSize = frame_count * sizeof(float);
        io_data->mBuffers[1].mData = (void *)e->cur_in_r;
        io_data->mBuffers[1].mDataByteSize = frame_count * sizeof(float);
    } else {
        std::memcpy(io_data->mBuffers[0].mData, e->cur_in_l, frame_count * sizeof(float));
        std::memcpy(io_data->mBuffers[1].mData, e->cur_in_r, frame_count * sizeof(float));
    }
    return noErr;
}

} // anonymous namespace

@interface JmoAuHost : NSObject {
@public
    std::unordered_map<uint32_t, std::unique_ptr<Entry>> entries;
    uint32_t next_id;
    os_unfair_lock lock;
}
@end

@implementation JmoAuHost

- (instancetype)init {
    if ((self = [super init])) {
        next_id = 1;
        lock = OS_UNFAIR_LOCK_INIT;
    }
    return self;
}

- (void)scanAndCallback:(au_scan_cb)cb context:(void *)ctx {
    static const uint32_t kTypes[] = {
        kAudioUnitType_Effect,
        kAudioUnitType_MusicDevice,
        kAudioUnitType_MusicEffect,
    };
    for (uint32_t type : kTypes) {
        AudioComponentDescription desc = {0, 0, 0, 0, 0};
        desc.componentType = type;
        AudioComponent comp = nullptr;
        while ((comp = AudioComponentFindNext(comp, &desc)) != nullptr) {
            AudioComponentDescription d;
            if (AudioComponentGetDescription(comp, &d) != noErr) continue;

            CFStringRef cf_name = nullptr;
            if (AudioComponentCopyName(comp, &cf_name) != noErr || !cf_name) continue;
            char name_buf[256] = {0};
            CFStringGetCString(cf_name, name_buf, sizeof(name_buf), kCFStringEncodingUTF8);
            CFRelease(cf_name);

            // Latence rapportée AVANT instantiation : on doit instancier brièvement.
            // Coût observé en prod : ~5ms par plugin (caché par macOS après
            // un premier scan). Cache disque persistant à ajouter en S1.5.
            uint32_t latency_samples = 0;
            NSError *err = nil;
            AUAudioUnit *probe = [[AUAudioUnit alloc] initWithComponentDescription:d
                                                                            options:0
                                                                              error:&err];
            if (probe && !err) {
                latency_samples = (uint32_t)lround(probe.latency * kSampleRate);
                probe = nil; // ARC release
            }
            // `providesUserInterface` retourne YES uniquement pour les AU v3
            // qui exposent un custom view controller. Les AU v2 (= la totalité
            // des AU Apple natifs, AmpliTube legacy, etc.) retournent NO mais
            // sont parfaitement affichables via AUGenericView. Comme on utilise
            // AUGenericView en fallback dans openEditor:, on annonce un editor
            // pour TOUS les AU. Quand on switchera vers requestViewController
            // pour les AU v3 modernes (S2), on raffinera.
            int has_editor = 1;

            cb(ctx, d.componentType, d.componentSubType, d.componentManufacturer,
               name_buf, latency_samples, has_editor);
        }
    }
}

- (uint32_t)loadType:(uint32_t)au_type
             subtype:(uint32_t)au_subtype
               manuf:(uint32_t)au_manuf
           maxFrames:(uint32_t)max_frames
             errBuf:(char *)err_buf
            errSize:(size_t)err_size {
    AudioComponentDescription desc = {0, 0, 0, 0, 0};
    desc.componentType = au_type;
    desc.componentSubType = au_subtype;
    desc.componentManufacturer = au_manuf;

    AudioComponent comp = AudioComponentFindNext(nullptr, &desc);
    if (!comp) {
        if (err_buf) std::snprintf(err_buf, err_size, "AU not found");
        return 0;
    }

    AudioComponentInstance inst = nullptr;
    OSStatus st = AudioComponentInstanceNew(comp, &inst);
    if (st != noErr || !inst) {
        if (err_buf) std::snprintf(err_buf, err_size, "AudioComponentInstanceNew failed: %d", (int)st);
        return 0;
    }

    // Format I/O : 48k stéréo float32 non-interleaved (= 2 buffers, 1 ch chacun).
    AudioStreamBasicDescription fmt = {0};
    fmt.mSampleRate = kSampleRate;
    fmt.mFormatID = kAudioFormatLinearPCM;
    fmt.mFormatFlags = kAudioFormatFlagIsFloat
                      | kAudioFormatFlagIsPacked
                      | kAudioFormatFlagIsNonInterleaved;
    fmt.mBytesPerPacket = sizeof(float);
    fmt.mFramesPerPacket = 1;
    fmt.mBytesPerFrame = sizeof(float);
    fmt.mChannelsPerFrame = 2;
    fmt.mBitsPerChannel = 32;

    st = AudioUnitSetProperty(inst, kAudioUnitProperty_StreamFormat,
                              kAudioUnitScope_Input, 0, &fmt, sizeof(fmt));
    if (st != noErr) {
        if (err_buf) std::snprintf(err_buf, err_size, "setFormat in: %d", (int)st);
        AudioComponentInstanceDispose(inst);
        return 0;
    }
    st = AudioUnitSetProperty(inst, kAudioUnitProperty_StreamFormat,
                              kAudioUnitScope_Output, 0, &fmt, sizeof(fmt));
    if (st != noErr) {
        if (err_buf) std::snprintf(err_buf, err_size, "setFormat out: %d", (int)st);
        AudioComponentInstanceDispose(inst);
        return 0;
    }

    UInt32 maxF = max_frames;
    AudioUnitSetProperty(inst, kAudioUnitProperty_MaximumFramesPerSlice,
                         kAudioUnitScope_Global, 0, &maxF, sizeof(maxF));

    // Pré-allouer l'Entry pour avoir un pointeur stable AVANT set du callback
    // (le callback C reçoit ce pointeur en refCon).
    auto entry = std::make_unique<Entry>();
    entry->au_inst = inst;
    entry->editor_window = nil;
    entry->desc = desc;
    entry->in_l.assign(max_frames, 0.0f);
    entry->in_r.assign(max_frames, 0.0f);
    entry->max_frames = max_frames;
    entry->sample_time = 0.0;
    entry->cur_in_l = nullptr;
    entry->cur_in_r = nullptr;
    entry->cur_n_frames = 0;

    AURenderCallbackStruct cb = {0};
    cb.inputProc = jmo_render_callback;
    cb.inputProcRefCon = entry.get();
    st = AudioUnitSetProperty(inst, kAudioUnitProperty_SetRenderCallback,
                              kAudioUnitScope_Input, 0, &cb, sizeof(cb));
    if (st != noErr) {
        if (err_buf) std::snprintf(err_buf, err_size, "setRenderCallback: %d", (int)st);
        AudioComponentInstanceDispose(inst);
        return 0;
    }

    st = AudioUnitInitialize(inst);
    if (st != noErr) {
        if (err_buf) std::snprintf(err_buf, err_size, "AudioUnitInitialize: %d", (int)st);
        AudioComponentInstanceDispose(inst);
        return 0;
    }

    Float64 latency_sec = 0;
    UInt32 latency_size = sizeof(latency_sec);
    AudioUnitGetProperty(inst, kAudioUnitProperty_Latency,
                         kAudioUnitScope_Global, 0, &latency_sec, &latency_size);
    entry->latency_samples = (uint32_t)lround(latency_sec * kSampleRate);

    os_unfair_lock_lock(&lock);
    uint32_t id_ = next_id++;
    entries.emplace(id_, std::move(entry));
    os_unfair_lock_unlock(&lock);
    return id_;
}

- (int)unload:(uint32_t)handle_id {
    os_unfair_lock_lock(&lock);
    auto it = entries.find(handle_id);
    if (it == entries.end()) {
        os_unfair_lock_unlock(&lock);
        return -1;
    }
    auto entry = std::move(it->second);
    entries.erase(it);
    os_unfair_lock_unlock(&lock);

    // Cleanup AU & window — hors lock pour éviter de tenir le lock pendant
    // dealloc lourd.
    if (entry->editor_window) {
        NSWindow *w = entry->editor_window;
        dispatch_async(dispatch_get_main_queue(), ^{
            [w close];
        });
    }
    if (entry->au_inst) {
        AudioUnitUninitialize(entry->au_inst);
        AudioComponentInstanceDispose(entry->au_inst);
    }
    return 0;
}

// Process appelé depuis le thread RT. PAS de lock. PAS d'alloc.
// L'invariant côté Rust : pas d'unload concurrent sur le même handle.
- (int)processHandle:(uint32_t)handle_id
                left:(float *)left
               right:(float *)right
              frames:(uint32_t)n_frames {
    auto it = entries.find(handle_id);
    if (it == entries.end()) return -1;
    Entry *e = it->second.get();
    if (n_frames > e->max_frames) return -2;

    // Snapshot input — sera lu par jmo_render_callback via refCon.
    std::memcpy(e->in_l.data(), left,  n_frames * sizeof(float));
    std::memcpy(e->in_r.data(), right, n_frames * sizeof(float));
    e->cur_in_l = e->in_l.data();
    e->cur_in_r = e->in_r.data();
    e->cur_n_frames = n_frames;

    // Buffer list de sortie stack-alloué (2 channels non-interleaved).
    uint8_t abl_storage[sizeof(AudioBufferList) + sizeof(AudioBuffer)];
    AudioBufferList *abl = (AudioBufferList *)abl_storage;
    abl->mNumberBuffers = 2;
    abl->mBuffers[0].mNumberChannels = 1;
    abl->mBuffers[0].mDataByteSize = n_frames * sizeof(float);
    abl->mBuffers[0].mData = left;
    abl->mBuffers[1].mNumberChannels = 1;
    abl->mBuffers[1].mDataByteSize = n_frames * sizeof(float);
    abl->mBuffers[1].mData = right;

    AudioTimeStamp ts = {0};
    ts.mFlags = kAudioTimeStampSampleTimeValid;
    ts.mSampleTime = e->sample_time;

    AudioUnitRenderActionFlags flags = 0;
    OSStatus st = AudioUnitRender(e->au_inst, &flags, &ts, 0, n_frames, abl);
    if (st != noErr) return -2;

    e->sample_time += n_frames;
    return 0;
}

- (uint32_t)latencyFor:(uint32_t)handle_id {
    auto it = entries.find(handle_id);
    return (it == entries.end()) ? 0 : it->second->latency_samples;
}

- (int)openEditor:(uint32_t)handle_id {
    auto it = entries.find(handle_id);
    if (it == entries.end()) return -1;
    Entry *e = it->second.get();
    if (e->editor_window) {
        // Déjà ouverte → bring to front + activate l'app sinon la window
        // reste derrière Chrome/Tauri/etc. (bug observé en test E2E).
        NSWindow *w = e->editor_window;
        dispatch_async(dispatch_get_main_queue(), ^{
            [NSApp activateIgnoringOtherApps:YES];
            [w makeKeyAndOrderFront:nil];
        });
        return 0;
    }

    // FIX (S1.4 hotfix#3) — on utilise AUGenericView pointée sur la MÊME
    // AudioComponentInstance que celle utilisée par AudioUnitRender pour le
    // processing audio. Plus de duplication d'instance → les sliders bougent
    // bien le son, et fermer/réouvrir la window préserve les paramètres
    // (l'instance reste vivante jusqu'à unload). Pour les AU v3 modernes,
    // S2 raffinera avec requestViewController pour récupérer le custom UI.
    AudioComponentInstance inst = e->au_inst;
    AudioComponentDescription desc = e->desc;
    dispatch_async(dispatch_get_main_queue(), ^{
        CFStringRef cf_name = nullptr;
        AudioComponent comp = AudioComponentFindNext(nullptr, &desc);
        if (comp) AudioComponentCopyName(comp, &cf_name);
        NSString *title = (__bridge_transfer NSString *)cf_name;
        if (!title) title = @"AU Plugin";

        NSRect rect = NSMakeRect(120, 120, 640, 480);
        NSWindow *win = [[NSWindow alloc]
            initWithContentRect:rect
                      styleMask:(NSWindowStyleMaskTitled |
                                 NSWindowStyleMaskClosable |
                                 NSWindowStyleMaskResizable)
                        backing:NSBackingStoreBuffered
                          defer:NO];
        [win setTitle:title];
        [win setReleasedWhenClosed:NO];

        AUGenericView *view = [[AUGenericView alloc] initWithAudioUnit:inst];
        if (view) {
            view.showsExpertParameters = YES;
            [win setContentView:view];
        } else {
            NSTextField *label = [[NSTextField alloc] initWithFrame:rect];
            [label setStringValue:@"Ce plugin ne fournit pas d'éditeur."];
            [label setEditable:NO];
            [label setBezeled:NO];
            [label setDrawsBackground:NO];
            [label setAlignment:NSTextAlignmentCenter];
            [win setContentView:label];
        }

        [win center];
        [NSApp activateIgnoringOtherApps:YES];
        [win makeKeyAndOrderFront:nil];
        e->editor_window = win;

        // À la fermeture par l'utilisateur, on libère juste la window —
        // PAS le plugin (params préservés pour la prochaine ouverture).
        [[NSNotificationCenter defaultCenter]
            addObserverForName:NSWindowWillCloseNotification
                        object:win
                         queue:nil
                    usingBlock:^(NSNotification *_Nonnull __unused note) {
            e->editor_window = nil;
        }];
    });
    return 0;
}

- (int)closeEditor:(uint32_t)handle_id {
    auto it = entries.find(handle_id);
    if (it == entries.end()) return -1;
    Entry *e = it->second.get();
    if (!e->editor_window) return 0;
    NSWindow *w = e->editor_window;
    dispatch_async(dispatch_get_main_queue(), ^{
        [w close];
    });
    return 0;
}

@end

// ---------- C API ----------

extern "C" {

void *au_host_create(void) {
    JmoAuHost *h = [[JmoAuHost alloc] init];
    return (__bridge_retained void *)h;
}

void au_host_destroy(void *p) {
    if (!p) return;
    (void)(__bridge_transfer JmoAuHost *)p; // ARC release et dealloc
}

void au_host_scan(void *p, au_scan_cb cb, void *ctx) {
    if (!p || !cb) return;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    [h scanAndCallback:cb context:ctx];
}

uint32_t au_host_load(void *p,
                      uint32_t au_type, uint32_t au_subtype, uint32_t au_manuf,
                      uint32_t max_frames,
                      char *err_buf, size_t err_size) {
    if (!p) return 0;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    return [h loadType:au_type subtype:au_subtype manuf:au_manuf
              maxFrames:max_frames errBuf:err_buf errSize:err_size];
}

int au_host_unload(void *p, uint32_t handle_id) {
    if (!p) return -1;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    return [h unload:handle_id];
}

int au_host_process_stereo(void *p, uint32_t handle_id,
                            float *left, float *right, uint32_t n_frames) {
    if (!p) return -1;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    return [h processHandle:handle_id left:left right:right frames:n_frames];
}

uint32_t au_host_latency_samples(void *p, uint32_t handle_id) {
    if (!p) return 0;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    return [h latencyFor:handle_id];
}

int au_host_open_editor(void *p, uint32_t handle_id) {
    if (!p) return -1;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    return [h openEditor:handle_id];
}

int au_host_close_editor(void *p, uint32_t handle_id) {
    if (!p) return -1;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    return [h closeEditor:handle_id];
}

// fourcc helper exposé pour debug.
void au_fourcc(uint32_t code, char out[5]) {
    fourcc(code, out);
}

} // extern "C"
