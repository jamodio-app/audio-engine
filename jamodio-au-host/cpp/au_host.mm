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
#import <AudioToolbox/AUCocoaUIView.h>
#import <AudioToolbox/MusicDevice.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreAudioKit/CoreAudioKit.h>
#import <CoreMIDI/CoreMIDI.h>
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
                           int has_editor,
                           int has_input_bus);  // S2 : 1 si bus audio in, 0 sinon

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

// État par plugin chargé. Path hybride v2/v3 :
//
// - **AU v2** (Apple natifs, AmpliTube legacy, plugins anciens) : API C
//   legacy `AudioComponentInstance` + `AudioUnitRender` + `AUGenericView`
//   pour la GUI. Une seule instance partagée GUI ↔ processing.
//
// - **AU v3** (AmpliTube 5, Battery 4, NIH-plug récents, suite NI/Korg
//   moderne) : API moderne `AUAudioUnit` + `renderBlock` +
//   `requestViewControllerWithCompletionHandler:` qui retourne l'UI
//   CUSTOM du plugin (la vraie interface graphique 3D amplis/cabs etc.).
//   Une seule instance AUAudioUnit partagée GUI ↔ processing.
//
// Détection au load via `componentFlags & kAudioComponentFlag_IsV3AudioUnit`.
struct Entry {
    bool is_v3;
    bool has_input_bus;                 // false pour les synthés MIDI purs
    // v2 path
    AudioComponentInstance au_inst;     // nullptr en v3
    // v3 path
    __strong AUAudioUnit *au_v3;        // nil en v2
    AURenderBlock render_block_v3;      // nullptr en v2
    // Commun
    __strong NSWindow *editor_window;   // nil tant que pas ouvert
    AudioComponentDescription desc;
    std::vector<float> in_l, in_r;      // copie de l'input par bloc (callback)
    uint32_t max_frames;
    uint32_t latency_samples;
    double sample_time;                 // monotonic pour timestamp render
    // État courant lu par render_callback/pull_block (set avant render)
    const float *cur_in_l;
    const float *cur_in_r;
    uint32_t cur_n_frames;
};

// S2 — Dispatch des events MIDI vers le plugin AVANT le render. Le format
// d'entrée (`data`) est un tableau de N*3 bytes ASCII MIDI : pour chaque
// event, status/data1/data2. Les events sont émis avec offset 0 (= début
// du bloc) — sample-precise timing = future S2+.

static void jmo_dispatch_midi_v2(AudioComponentInstance au_inst,
                                 const uint8_t *data,
                                 uint32_t count) {
    if (!au_inst || !data) return;
    for (uint32_t i = 0; i < count; i++) {
        UInt32 status = data[i * 3 + 0];
        UInt32 d1     = data[i * 3 + 1];
        UInt32 d2     = data[i * 3 + 2];
        // MusicDeviceMIDIEvent : route un voice channel message au plugin.
        // offsetSampleFrame=0 → traité au début du prochain AudioUnitRender.
        MusicDeviceMIDIEvent(au_inst, status, d1, d2, 0);
    }
}

static void jmo_dispatch_midi_v3(AUAudioUnit *au_v3,
                                 const uint8_t *data,
                                 uint32_t count) {
    if (!au_v3 || !data || count == 0) return;
    AUMIDIEventListBlock block = au_v3.scheduleMIDIEventListBlock;
    if (!block) return;

    // Build un MIDIEventList avec les N events au format UMP MIDI 1.0
    // (1 word de 32 bits par event voice). MIDIEventListAdd alloue dans
    // notre buffer (= taille = sizeof(list) + N * sizeof(packet)).
    size_t list_size = sizeof(MIDIEventList) + count * sizeof(MIDIEventPacket);
    void *buf = std::malloc(list_size);
    if (!buf) return;
    MIDIEventList *list = (MIDIEventList *)buf;
    MIDIEventPacket *packet = MIDIEventListInit(list, kMIDIProtocol_1_0);

    for (uint32_t i = 0; i < count; i++) {
        uint8_t status = data[i * 3 + 0];
        uint8_t d1     = data[i * 3 + 1];
        uint8_t d2     = data[i * 3 + 2];
        // UMP MIDI 1.0 voice message : 1 word de 32 bits.
        //   bit 31-28 : MT (Message Type) = 0x2 (MIDI 1.0)
        //   bit 27-24 : group (0..15, on prend 0)
        //   bit 23-16 : status byte (incluant channel)
        //   bit 15-8  : data1
        //   bit 7-0   : data2
        UInt32 word = ((UInt32)0x2 << 28)
                    | ((UInt32)0 << 24)
                    | ((UInt32)status << 16)
                    | ((UInt32)d1 << 8)
                    | ((UInt32)d2);
        packet = MIDIEventListAdd(list, (ByteCount)list_size, packet, 0, 1, &word);
        if (!packet) break;
    }

    // AUEventSampleTimeImmediate = délivrer ASAP (= dans le prochain render).
    block((AUEventSampleTime)0xFFFFFFFF00000000LL, 0, list);
    std::free(buf);
}

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

// v0.2.23 — Exécute `block` sur le main thread Cocoa, synchrone.
//
// Pourquoi : `AudioComponentInstanceNew` + `AUAudioUnit alloc init` doivent
// tourner sur un thread avec une CFRunLoop active. Les plugins lourds
// (BFD Player, AmpliTube 5, Kontakt, beaucoup d'autres) font du XPC sync
// vers leur daemon de licence / sample engine à l'instanciation, et ce XPC
// ne renvoie que si un runloop pompe les messages. Notre WS handler tourne
// sur un tokio worker sans runloop → InstanceNew retourne -1 silencieusement
// (cas observé en prod 2026-05-13 sur Mac M-series de Yannick).
//
// La doc Apple Audio Unit recommande explicitement le main thread pour
// load/init/uninitialize (HostAudioUnits Programming Guide).
//
// Implémentation : on utilise `dispatch_async` + `dispatch_semaphore_wait`
// avec timeout, plutôt que `dispatch_sync`. Raison : en environnement de
// test Rust (cargo test), le main thread est le test runner qui ne pompe
// PAS la main queue → `dispatch_sync` deadlock infini. Avec un timeout,
// on tombe en fallback "exécution inline" sur le tokio worker, ce qui
// suffit pour les plugins simples (Apple natifs, EQ basique) testés en CI.
//
// En production (Tauri = NSApp avec runloop main pompée), le block est
// exécuté quasi instantanément (~50 µs), le timeout n'est jamais atteint.
//
// Garantie d'exécution unique via flag `done` + os_unfair_lock : si on
// timeout sur le wait et qu'on exécute inline, ET que le block dispatch_async
// finit par être pompé plus tard, le second appel ne s'exécute pas.
static void jmo_run_on_main_sync(dispatch_block_t block) {
    if ([NSThread isMainThread]) {
        block();
        return;
    }
    // Détection contexte test/CLI : NSApp est initialisé par Tauri au démarrage
    // de l'agent (NSApplication sharedApplication). En `cargo test` ou en
    // contexte CLI sans GUI, NSApp reste nil → la main queue n'est pas pompée
    // → dispatch_sync deadlock. On exécute inline dans ce cas.
    // En production (Tauri actif), NSApp != nil → dispatch_async + wait OK.
    if (NSApp == nil) {
        block();
        return;
    }
    // 10 secondes = largement assez pour les plugins les plus lents
    // (BFD = 2.3 s cold open, mesuré via auval). Au-delà = main thread KO,
    // on fallback inline.
    static constexpr long long TIMEOUT_NS = 10LL * NSEC_PER_SEC;

    __block bool done = false;
    __block os_unfair_lock done_lock = OS_UNFAIR_LOCK_INIT;
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);

    dispatch_async(dispatch_get_main_queue(), ^{
        bool should_run = false;
        os_unfair_lock_lock(&done_lock);
        if (!done) { done = true; should_run = true; }
        os_unfair_lock_unlock(&done_lock);
        if (should_run) block();
        dispatch_semaphore_signal(sem);
    });

    dispatch_time_t deadline = dispatch_time(DISPATCH_TIME_NOW, TIMEOUT_NS);
    if (dispatch_semaphore_wait(sem, deadline) != 0) {
        // Timeout : main thread ne pompe pas la queue. Exécution inline
        // (= tokio worker thread). Pour les plugins simples ça marche.
        // Pour les plugins exigeants (cf. BFD/AmpliTube), on récupère le
        // comportement v0.2.22 (échec InstanceNew -1) — au moins on ne
        // pend pas indéfiniment.
        bool should_run = false;
        os_unfair_lock_lock(&done_lock);
        if (!done) { done = true; should_run = true; }
        os_unfair_lock_unlock(&done_lock);
        if (should_run) block();
        // Le block originel reste queued sur main et fera no-op au pickup
        // éventuel grâce au flag `done`. Le `sem` se libère par ARC quand
        // le block s'exécute ou est dropé.
    }
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
    // S1.9 — Scan multi-types : effets (`aufx`) + instruments (`aumu`) +
    // music-effects (`aumf`). On laisse le load filtrer plus finement selon
    // la présence d'un bus audio in (cf. loadType:). Raison : certains
    // plugins listés en `aumu` ont quand même un bus audio in (cas
    // AmpliTube 5 chez IK Multimedia) → exclure tout `aumu` cachait
    // AmpliTube de la liste alors qu'il marchait avant. Les vrais
    // synthés MIDI purs (AUMIDISynth, AUSampler) restent chargeables
    // au MVP mais produiront silence sans MIDI → MIDI routing arrive en S2.
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
            int has_input_bus = 1;
            NSError *err = nil;
            AUAudioUnit *probe = [[AUAudioUnit alloc] initWithComponentDescription:d
                                                                            options:0
                                                                              error:&err];
            if (probe && !err) {
                latency_samples = (uint32_t)lround(probe.latency * kSampleRate);
                // S2 — has_input_bus permet au browser de savoir s'il faut
                // auto-switcher en source MIDI (= pur instrument) au load.
                has_input_bus = (probe.inputBusses.count > 0) ? 1 : 0;
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
               name_buf, latency_samples, has_editor, has_input_bus);
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

    // Détection v3 via le flag dans la description du composant. Les AU v3
    // ont obligatoirement `kAudioComponentFlag_IsV3AudioUnit` (= 64) dans
    // leurs componentFlags. AmpliTube 5, Battery 4, plugins NIH-plug
    // récents = v3. Apple natifs, AmpliTube legacy = v2.
    AudioComponentDescription d;
    AudioComponentGetDescription(comp, &d);
    const bool prefer_v3 = (d.componentFlags & kAudioComponentFlag_IsV3AudioUnit) != 0;

    auto entry = std::make_unique<Entry>();
    entry->is_v3 = false;               // sera fixé par le chemin qui réussit
    entry->has_input_bus = false;       // sera mis à true plus bas si l'AU a un input bus
    entry->au_inst = nullptr;
    entry->au_v3 = nil;
    entry->render_block_v3 = nullptr;
    entry->editor_window = nil;
    entry->desc = desc;
    entry->in_l.assign(max_frames, 0.0f);
    entry->in_r.assign(max_frames, 0.0f);
    entry->max_frames = max_frames;
    entry->sample_time = 0.0;
    entry->cur_in_l = nullptr;
    entry->cur_in_r = nullptr;
    entry->cur_n_frames = 0;

    // v0.2.23 — Tentative v3 wrappée dans un bloc dispatch main thread. Renvoie
    // true si le plugin est chargé (entry->au_v3 set). Sinon, écrit l'erreur
    // dans err_buf et laisse entry intact pour permettre un fallback v2.
    Entry *entry_raw = entry.get();
    AudioComponentDescription desc_for_v3 = desc;
    uint32_t max_frames_capture = max_frames;
    // Wrap dans une struct : ObjC blocks ne peuvent pas capturer un C array
    // avec __block (déclaration "of array type" interdite par le compilo).
    struct ErrSlot { char data[256]; };
    auto try_v3 = [&]() -> bool {
        __block bool ok = false;
        __block ErrSlot local_err = {{0}};
        jmo_run_on_main_sync(^{
            NSError *err = nil;
            AUAudioUnit *au = [[AUAudioUnit alloc] initWithComponentDescription:desc_for_v3
                                                                         options:0
                                                                           error:&err];
            if (!au || err) {
                std::snprintf(local_err.data, sizeof(local_err.data), "v3 init: %s",
                              err.localizedDescription.UTF8String);
                return;
            }
            AVAudioFormat *fmt = [[AVAudioFormat alloc]
                initStandardFormatWithSampleRate:kSampleRate channels:2];
            // S1.9 — n'ouvrir le bus input que s'il existe. Les synthés MIDI purs
            // (AUMIDISynth, AUSampler) ont 0 bus input → on les charge quand même
            // pour permettre l'ouverture de leur éditeur, mais ils produiront
            // silence tant qu'on n'a pas de routing MIDI in.
            entry_raw->has_input_bus = (au.inputBusses.count > 0);
            if (entry_raw->has_input_bus) {
                for (AUAudioUnitBus *bus in au.inputBusses) {
                    bus.enabled = YES;
                    if (![bus setFormat:fmt error:&err]) {
                        std::snprintf(local_err.data, sizeof(local_err.data), "v3 setFormat in: %s",
                                      err.localizedDescription.UTF8String);
                        return;
                    }
                }
            }
            for (AUAudioUnitBus *bus in au.outputBusses) {
                bus.enabled = YES;
                if (![bus setFormat:fmt error:&err]) {
                    std::snprintf(local_err.data, sizeof(local_err.data), "v3 setFormat out: %s",
                                  err.localizedDescription.UTF8String);
                    return;
                }
            }
            au.maximumFramesToRender = max_frames_capture;
            if (![au allocateRenderResourcesAndReturnError:&err]) {
                std::snprintf(local_err.data, sizeof(local_err.data), "v3 allocRender: %s",
                              err.localizedDescription.UTF8String);
                return;
            }
            entry_raw->au_v3 = au;
            entry_raw->render_block_v3 = au.renderBlock;
            entry_raw->latency_samples = (uint32_t)lround(au.latency * kSampleRate);
            entry_raw->is_v3 = true;
            ok = true;
        });
        if (!ok && err_buf && local_err.data[0]) {
            std::snprintf(err_buf, err_size, "%s", local_err.data);
        }
        return ok;
    };

    // v0.2.23 — Tentative v2 wrappée dans un bloc dispatch main thread. Idem.
    // C'est ce chemin qui plantait avec "InstanceNew failed: -1" sur les plugins
    // lourds (BFD, AmpliTube) appelés depuis un thread sans CFRunLoop active.
    AudioComponent comp_for_v2 = comp;
    auto try_v2 = [&]() -> bool {
        __block bool ok = false;
        __block ErrSlot local_err = {{0}};
        jmo_run_on_main_sync(^{
            AudioComponentInstance inst = nullptr;
            OSStatus st = AudioComponentInstanceNew(comp_for_v2, &inst);
            if (st != noErr || !inst) {
                std::snprintf(local_err.data, sizeof(local_err.data),
                              "v2 InstanceNew failed: %d", (int)st);
                return;
            }

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

            // S1.9 — détection du nombre de bus input via kAudioUnitProperty_ElementCount.
            // Les synthés MIDI purs en v2 (AUMIDISynth, AUSampler) ont 0 bus input
            // → on doit skip setFormat/setRenderCallback sur scope:Input (sinon
            // -10877 InvalidScope).
            UInt32 n_in_bus = 0;
            UInt32 sz = sizeof(n_in_bus);
            AudioUnitGetProperty(inst, kAudioUnitProperty_ElementCount,
                                 kAudioUnitScope_Input, 0, &n_in_bus, &sz);
            entry_raw->has_input_bus = (n_in_bus > 0);

            st = AudioUnitSetProperty(inst, kAudioUnitProperty_StreamFormat,
                                      kAudioUnitScope_Output, 0, &fmt, sizeof(fmt));
            if (st != noErr) {
                std::snprintf(local_err.data, sizeof(local_err.data),
                              "v2 setFormat out: %d", (int)st);
                AudioComponentInstanceDispose(inst);
                return;
            }

            if (entry_raw->has_input_bus) {
                st = AudioUnitSetProperty(inst, kAudioUnitProperty_StreamFormat,
                                          kAudioUnitScope_Input, 0, &fmt, sizeof(fmt));
                if (st != noErr) {
                    std::snprintf(local_err.data, sizeof(local_err.data),
                                  "v2 setFormat in: %d", (int)st);
                    AudioComponentInstanceDispose(inst);
                    return;
                }
            }

            UInt32 maxF = max_frames_capture;
            AudioUnitSetProperty(inst, kAudioUnitProperty_MaximumFramesPerSlice,
                                 kAudioUnitScope_Global, 0, &maxF, sizeof(maxF));

            if (entry_raw->has_input_bus) {
                AURenderCallbackStruct cb = {0};
                cb.inputProc = jmo_render_callback;
                cb.inputProcRefCon = entry_raw;
                st = AudioUnitSetProperty(inst, kAudioUnitProperty_SetRenderCallback,
                                          kAudioUnitScope_Input, 0, &cb, sizeof(cb));
                if (st != noErr) {
                    std::snprintf(local_err.data, sizeof(local_err.data),
                                  "v2 setRenderCallback: %d", (int)st);
                    AudioComponentInstanceDispose(inst);
                    return;
                }
            }

            st = AudioUnitInitialize(inst);
            if (st != noErr) {
                std::snprintf(local_err.data, sizeof(local_err.data),
                              "v2 AudioUnitInitialize: %d", (int)st);
                AudioComponentInstanceDispose(inst);
                return;
            }

            Float64 latency_sec = 0;
            UInt32 latency_size = sizeof(latency_sec);
            AudioUnitGetProperty(inst, kAudioUnitProperty_Latency,
                                 kAudioUnitScope_Global, 0, &latency_sec, &latency_size);
            entry_raw->latency_samples = (uint32_t)lround(latency_sec * kSampleRate);
            entry_raw->au_inst = inst;
            entry_raw->is_v3 = false;
            ok = true;
        });
        if (!ok && err_buf && local_err.data[0]) {
            std::snprintf(err_buf, err_size, "%s", local_err.data);
        }
        return ok;
    };

    // v0.2.23 — Stratégie : tenter le chemin "préféré" selon le flag du composant,
    // puis fallback sur l'autre. Beaucoup de plugins publient les deux interfaces
    // (v2 + v3) avec des comportements différents — l'un peut planter là où
    // l'autre passe. Côté Yannick (M-series, macOS 15.7.5), v2 InstanceNew
    // retournait -1 pour BFD/AmpliTube ; le fallback v3 a de bonnes chances de
    // marcher. Inversement, certains plugins v3 mal écrits crashent en init et
    // le fallback v2 sauve la journée.
    bool ok = false;
    char primary_err[256] = {0};
    if (prefer_v3) {
        ok = try_v3();
        if (!ok) {
            if (err_buf) std::snprintf(primary_err, sizeof(primary_err), "%s", err_buf);
            ok = try_v2();
            if (ok && err_buf) {
                // Plugin chargé via fallback — efface l'erreur primaire pour
                // que le caller ne se croie pas en échec.
                err_buf[0] = 0;
            }
        }
    } else {
        ok = try_v2();
        if (!ok) {
            if (err_buf) std::snprintf(primary_err, sizeof(primary_err), "%s", err_buf);
            ok = try_v3();
            if (ok && err_buf) {
                err_buf[0] = 0;
            }
        }
    }
    if (!ok) {
        // Les deux chemins ont échoué. err_buf contient l'erreur du dernier essai,
        // on annexe l'erreur du premier essai pour aider le diag.
        if (err_buf && primary_err[0]) {
            char last_err[256] = {0};
            std::snprintf(last_err, sizeof(last_err), "%s", err_buf);
            std::snprintf(err_buf, err_size, "%s (fallback: %s)",
                          primary_err, last_err);
        }
        return 0;
    }

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
    // dealloc lourd. Branche selon le path utilisé au load.
    if (entry->editor_window) {
        NSWindow *w = entry->editor_window;
        dispatch_async(dispatch_get_main_queue(), ^{
            [w close];
        });
    }
    if (entry->is_v3) {
        [entry->au_v3 deallocateRenderResources];
        entry->au_v3 = nil; // ARC
        entry->render_block_v3 = nullptr;
    } else if (entry->au_inst) {
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

    // Snapshot input — lu par le callback (v2) ou le pull_block (v3).
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

    OSStatus st;
    if (e->is_v3) {
        // Path v3 : renderBlock + AURenderPullInputBlock fournit l'input.
        // Si pas de bus input (synthé MIDI pur), pull = nil → renderBlock
        // produit son output naturel (silence sans MIDI au MVP).
        AURenderPullInputBlock pull = nil;
        if (e->has_input_bus) {
            pull = ^OSStatus(AudioUnitRenderActionFlags *,
                             const AudioTimeStamp *,
                             AUAudioFrameCount frame_count,
                             NSInteger /*bus*/,
                             AudioBufferList *in_data) {
                if (in_data->mNumberBuffers < 2) return -1;
                if (frame_count > e->cur_n_frames) return -1;
                if (in_data->mBuffers[0].mData == nullptr) {
                    in_data->mBuffers[0].mData = (void *)e->cur_in_l;
                    in_data->mBuffers[0].mDataByteSize = frame_count * sizeof(float);
                    in_data->mBuffers[1].mData = (void *)e->cur_in_r;
                    in_data->mBuffers[1].mDataByteSize = frame_count * sizeof(float);
                } else {
                    std::memcpy(in_data->mBuffers[0].mData, e->cur_in_l, frame_count * sizeof(float));
                    std::memcpy(in_data->mBuffers[1].mData, e->cur_in_r, frame_count * sizeof(float));
                }
                return noErr;
            };
        }
        st = e->render_block_v3(&flags, &ts, n_frames, 0, abl, pull);
    } else {
        // Path v2 : AudioUnitRender. Si pas de bus input, le callback C n'a
        // pas été set au load, AudioUnitRender produira l'output naturel.
        st = AudioUnitRender(e->au_inst, &flags, &ts, 0, n_frames, abl);
    }
    if (st != noErr) return -2;

    e->sample_time += n_frames;
    return 0;
}

- (uint32_t)latencyFor:(uint32_t)handle_id {
    auto it = entries.find(handle_id);
    return (it == entries.end()) ? 0 : it->second->latency_samples;
}

// S2 — Dispatche des events MIDI vers le plugin. Appelé depuis le thread
// audio RT (encoder_thread) JUSTE AVANT processHandle:. Sans lock (cf.
// invariant : pas d'unload concurrent sur le même handle).
- (void)dispatchMidi:(const uint8_t *)data
               count:(uint32_t)count
            toHandle:(uint32_t)handle_id {
    if (count == 0 || !data) return;
    auto it = entries.find(handle_id);
    if (it == entries.end()) return;
    Entry *e = it->second.get();
    if (e->is_v3) {
        jmo_dispatch_midi_v3(e->au_v3, data, count);
    } else {
        jmo_dispatch_midi_v2(e->au_inst, data, count);
    }
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

    // Path hybride (S1.7) :
    // - AU v3 → requestViewControllerWithCompletionHandler: → UI custom du
    //   plugin (vraie interface graphique AmpliTube, Battery, etc.)
    // - AU v2 → AUGenericView initWithAudioUnit: → sliders génériques sur
    //   la même instance que le processing (params sync GUI ↔ son).
    // Dans les deux cas, fermer la window ne dispose pas le plugin → params
    // persistent jusqu'au unload explicite.
    AudioComponentDescription desc = e->desc;
    if (e->is_v3) {
        __strong AUAudioUnit *au_v3 = e->au_v3;
        dispatch_async(dispatch_get_main_queue(), ^{
            [au_v3 requestViewControllerWithCompletionHandler:^(AUViewControllerBase * _Nullable vc) {
                dispatch_async(dispatch_get_main_queue(), ^{
                    NSView *view = vc ? vc.view : nil;
                    CGSize prefSize = view ? view.frame.size : CGSizeMake(800, 600);
                    if (prefSize.width < 200 || prefSize.height < 100) {
                        prefSize = CGSizeMake(800, 600);
                    }
                    NSRect rect = NSMakeRect(120, 120, prefSize.width, prefSize.height);
                    // S1.11 — Window NON resizable (convention DAW Logic/Ableton/Reaper).
                    // Les UI plugins ont une taille fixe imposée par le plugin (AmpliTube,
                    // TONEX et 95% des plugins commerciaux). Laisser le user resize la
                    // window créait une zone vide trompeuse autour de l'UI fixe.
                    NSWindow *win = [[NSWindow alloc]
                        initWithContentRect:rect
                                  styleMask:(NSWindowStyleMaskTitled |
                                             NSWindowStyleMaskClosable)
                                    backing:NSBackingStoreBuffered
                                      defer:NO];
                    [win setTitle:(au_v3.audioUnitName ?: @"AU Plugin")];
                    [win setReleasedWhenClosed:NO];
                    if (view) {
                        view.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
                        [win setContentView:view];
                    } else {
                        NSTextField *label = [[NSTextField alloc] initWithFrame:rect];
                        [label setStringValue:@"Ce plugin v3 ne fournit pas d'éditeur."];
                        [label setEditable:NO];
                        [label setBezeled:NO];
                        [label setDrawsBackground:NO];
                        [label setAlignment:NSTextAlignmentCenter];
                        label.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
                        [win setContentView:label];
                    }
                    [win center];
                    [NSApp activateIgnoringOtherApps:YES];
                    [win makeKeyAndOrderFront:nil];
                    e->editor_window = win;
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

    // Path v2 — essayer d'abord le custom Cocoa UI exposé par le plugin via
    // `kAudioUnitProperty_CocoaUI`. C'est l'API officielle des hôtes pro pour
    // les AU v2 modernes (AmpliTube 5, plein de plugins commerciaux récents
    // restés en v2 legacy malgré leur modernité). Fallback sur AUGenericView
    // (sliders bruts) si le plugin ne fournit pas de Cocoa UI.
    AudioComponentInstance inst = e->au_inst;
    dispatch_async(dispatch_get_main_queue(), ^{
        CFStringRef cf_name = nullptr;
        AudioComponent comp = AudioComponentFindNext(nullptr, &desc);
        if (comp) AudioComponentCopyName(comp, &cf_name);
        NSString *title = (__bridge_transfer NSString *)cf_name;
        if (!title) title = @"AU Plugin";

        // Tentative Cocoa UI custom (v2).
        NSView *customView = nil;
        UInt32 dataSize = 0;
        Boolean isWritable = NO;
        OSStatus st = AudioUnitGetPropertyInfo(inst, kAudioUnitProperty_CocoaUI,
                                               kAudioUnitScope_Global, 0,
                                               &dataSize, &isWritable);
        if (st == noErr && dataSize >= sizeof(AudioUnitCocoaViewInfo)) {
            AudioUnitCocoaViewInfo *cocoaUI =
                (AudioUnitCocoaViewInfo *)std::malloc(dataSize);
            if (cocoaUI) {
                OSStatus st2 = AudioUnitGetProperty(inst, kAudioUnitProperty_CocoaUI,
                                                    kAudioUnitScope_Global, 0,
                                                    cocoaUI, &dataSize);
                if (st2 == noErr) {
                    NSURL *bundleURL = (__bridge NSURL *)cocoaUI->mCocoaAUViewBundleLocation;
                    NSString *className = (__bridge NSString *)cocoaUI->mCocoaAUViewClass[0];
                    if (bundleURL && className) {
                        NSBundle *viewBundle = [NSBundle bundleWithURL:bundleURL];
                        if ([viewBundle load]) {
                            Class viewFactoryClass = [viewBundle classNamed:className];
                            if (viewFactoryClass &&
                                [viewFactoryClass conformsToProtocol:@protocol(AUCocoaUIBase)]) {
                                id<AUCocoaUIBase> factory = [[viewFactoryClass alloc] init];
                                customView = [factory uiViewForAudioUnit:inst
                                                                withSize:NSMakeSize(800, 600)];
                            }
                        }
                    }
                }
                // Cleanup CF refs créées par AudioUnitGetProperty.
                if (cocoaUI->mCocoaAUViewBundleLocation) {
                    CFRelease(cocoaUI->mCocoaAUViewBundleLocation);
                }
                UInt32 count = (dataSize - sizeof(CFURLRef)) / sizeof(CFStringRef);
                for (UInt32 i = 0; i < count; i++) {
                    if (cocoaUI->mCocoaAUViewClass[i]) {
                        CFRelease(cocoaUI->mCocoaAUViewClass[i]);
                    }
                }
                std::free(cocoaUI);
            }
        }

        // Taille window adaptée à la view fournie (sinon 640x480).
        NSSize prefSize = customView ? customView.frame.size : NSMakeSize(640, 480);
        if (prefSize.width < 200 || prefSize.height < 100) {
            prefSize = NSMakeSize(640, 480);
        }
        NSRect rect = NSMakeRect(120, 120, prefSize.width, prefSize.height);
        // S1.11 — Window NON resizable (convention DAW). Cf. path v3 ci-dessus.
        NSWindow *win = [[NSWindow alloc]
            initWithContentRect:rect
                      styleMask:(NSWindowStyleMaskTitled |
                                 NSWindowStyleMaskClosable)
                        backing:NSBackingStoreBuffered
                          defer:NO];
        [win setTitle:title];
        [win setReleasedWhenClosed:NO];

        if (customView) {
            customView.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
            [win setContentView:customView];
        } else {
            AUGenericView *view = [[AUGenericView alloc] initWithAudioUnit:inst];
            if (view) {
                view.showsExpertParameters = YES;
                view.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
                [win setContentView:view];
            } else {
                NSTextField *label = [[NSTextField alloc] initWithFrame:rect];
                [label setStringValue:@"Ce plugin ne fournit pas d'éditeur."];
                [label setEditable:NO];
                [label setBezeled:NO];
                [label setDrawsBackground:NO];
                [label setAlignment:NSTextAlignmentCenter];
                label.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
                [win setContentView:label];
            }
        }

        [win center];
        [NSApp activateIgnoringOtherApps:YES];
        [win makeKeyAndOrderFront:nil];
        e->editor_window = win;
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

// S2 — Dispatche un batch d'events MIDI au plugin AVANT le prochain
// process_stereo. `midi_data` = N * 3 bytes (status, data1, data2). No-op
// si count=0. Doit être appelé sur le thread RT, juste avant
// au_host_process_stereo dans la boucle encoder.
void au_host_dispatch_midi(void *p, uint32_t handle_id,
                            const uint8_t *midi_data, uint32_t midi_count) {
    if (!p) return;
    JmoAuHost *h = (__bridge JmoAuHost *)p;
    [h dispatchMidi:midi_data count:midi_count toHandle:handle_id];
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
