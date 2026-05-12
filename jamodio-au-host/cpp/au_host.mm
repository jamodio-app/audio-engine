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
struct Entry {
    __strong AUAudioUnit *au;
    __strong NSWindow *editor_window; // nil tant que pas ouvert
    AudioComponentDescription desc;
    AURenderBlock render_block;          // copié hors d'AUAudioUnit pour appel direct
    std::vector<float> in_l, in_r;       // copie de l'input par bloc (pull_block)
    uint32_t max_frames;
    uint32_t latency_samples;
    double sample_time;                  // monotonic pour timestamp render
    // État courant utilisé par pull_block (set juste avant renderBlock)
    const float *cur_in_l;
    const float *cur_in_r;
    uint32_t cur_n_frames;
};

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

    NSError *err = nil;
    AUAudioUnit *au = [[AUAudioUnit alloc] initWithComponentDescription:desc
                                                                 options:0
                                                                   error:&err];
    if (!au || err) {
        if (err_buf) std::snprintf(err_buf, err_size, "init: %s",
                                    err.localizedDescription.UTF8String);
        return 0;
    }

    AVAudioFormat *fmt = [[AVAudioFormat alloc]
        initStandardFormatWithSampleRate:kSampleRate channels:2];
    for (AUAudioUnitBus *bus in au.inputBusses) {
        bus.enabled = YES;
        if (![bus setFormat:fmt error:&err]) {
            if (err_buf) std::snprintf(err_buf, err_size, "setFormat in: %s",
                                        err.localizedDescription.UTF8String);
            return 0;
        }
    }
    for (AUAudioUnitBus *bus in au.outputBusses) {
        bus.enabled = YES;
        if (![bus setFormat:fmt error:&err]) {
            if (err_buf) std::snprintf(err_buf, err_size, "setFormat out: %s",
                                        err.localizedDescription.UTF8String);
            return 0;
        }
    }
    au.maximumFramesToRender = max_frames;

    if (![au allocateRenderResourcesAndReturnError:&err]) {
        if (err_buf) std::snprintf(err_buf, err_size, "allocRenderResources: %s",
                                    err.localizedDescription.UTF8String);
        return 0;
    }

    auto entry = std::make_unique<Entry>();
    entry->au = au;
    entry->editor_window = nil;
    entry->desc = desc;
    entry->render_block = au.renderBlock;
    entry->in_l.assign(max_frames, 0.0f);
    entry->in_r.assign(max_frames, 0.0f);
    entry->max_frames = max_frames;
    entry->latency_samples = (uint32_t)lround(au.latency * kSampleRate);
    entry->sample_time = 0.0;
    entry->cur_in_l = nullptr;
    entry->cur_in_r = nullptr;
    entry->cur_n_frames = 0;

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
    // dealloc lourd. ARC s'occupe des objets ObjC.
    if (entry->editor_window) {
        // Fermeture window sur main thread.
        NSWindow *w = entry->editor_window;
        dispatch_async(dispatch_get_main_queue(), ^{
            [w close];
        });
    }
    [entry->au deallocateRenderResources];
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

    // Snapshot input pour le pull_block (qui sera invoqué par renderBlock).
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

    AURenderPullInputBlock pull = ^OSStatus(AudioUnitRenderActionFlags *flags,
                                            const AudioTimeStamp *t,
                                            AUAudioFrameCount frame_count,
                                            NSInteger bus_number,
                                            AudioBufferList *in_data) {
        if (in_data->mNumberBuffers < 2) return -1;
        const uint32_t want = (uint32_t)frame_count;
        if (want > e->cur_n_frames) return -1;
        if (in_data->mBuffers[0].mData == nullptr) {
            in_data->mBuffers[0].mData = (void *)e->cur_in_l;
            in_data->mBuffers[0].mDataByteSize = want * sizeof(float);
            in_data->mBuffers[1].mData = (void *)e->cur_in_r;
            in_data->mBuffers[1].mDataByteSize = want * sizeof(float);
        } else {
            std::memcpy(in_data->mBuffers[0].mData, e->cur_in_l, want * sizeof(float));
            std::memcpy(in_data->mBuffers[1].mData, e->cur_in_r, want * sizeof(float));
        }
        return noErr;
    };

    AudioUnitRenderActionFlags flags = 0;
    OSStatus st = e->render_block(&flags, &ts, n_frames, 0, abl, pull);
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

    // FIX (S1.4 hotfix#2) — utilise UNE seule instance AUAudioUnit pour le
    // processing audio ET la GUI. Avant on créait une 2e AudioComponentInstance
    // dédiée à AUGenericView, ce qui faisait :
    //   • les paramètres bougés dans la GUI n'affectaient pas le son
    //     (= la GUI parlait à l'instance morte, le renderBlock à l'autre)
    //   • à la fermeture de la window on disposait l'instance GUI →
    //     paramètres reset à la prochaine ouverture.
    // Maintenant on demande au plugin son viewController via
    // requestViewControllerWithCompletionHandler:, qui retourne :
    //   • AU v3 : le custom UI du plugin
    //   • AU v2 (Apple natifs, AmpliTube legacy) : controller générique
    //     d'Apple qui affiche les params, LIÉ à la même instance audio.
    // Plus de disposal à la fermeture — l'AUAudioUnit reste vivante tant
    // que le plugin est chargé, donc params persistent entre ouvertures.
    __strong AUAudioUnit *au_strong = e->au;
    dispatch_async(dispatch_get_main_queue(), ^{
        [au_strong requestViewControllerWithCompletionHandler:^(AUViewControllerBase * _Nullable vc) {
            dispatch_async(dispatch_get_main_queue(), ^{
                NSView *view = vc ? vc.view : nil;
                CGSize prefSize = view ? view.frame.size : CGSizeMake(640, 480);
                if (prefSize.width < 200 || prefSize.height < 100) {
                    prefSize = CGSizeMake(640, 480);
                }

                NSWindow *win = [[NSWindow alloc]
                    initWithContentRect:NSMakeRect(120, 120, prefSize.width, prefSize.height)
                              styleMask:(NSWindowStyleMaskTitled |
                                         NSWindowStyleMaskClosable |
                                         NSWindowStyleMaskResizable)
                                backing:NSBackingStoreBuffered
                                  defer:NO];
                NSString *title = au_strong.audioUnitName ?: @"AU Plugin";
                [win setTitle:title];
                [win setReleasedWhenClosed:NO];

                if (view) {
                    [win setContentView:view];
                } else {
                    NSTextField *label = [[NSTextField alloc]
                        initWithFrame:NSMakeRect(0, 0, prefSize.width, prefSize.height)];
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
