// audio_workgroup.mm
// ─────────────────────────────────────────────────────────────
// Sprint S2 (PLAN-EXECUTION-AGENT-STABILITE.md §S2.1) — bindings
// CoreAudio Workgroup pour scheduling cohérent du thread encoder
// avec le HAL audio macOS. Sans cette intégration, l'encoder_thread
// tourne en SCHED_OTHER nice value (que Darwin ignore largement),
// ce qui le rend préemptible par n'importe quel autre process →
// spikes 10-25 ms observés en baseline v0.4.1.
//
// API exposée (cf. workgroup.rs côté Rust) :
//   jamodio_audio_workgroup_available()         → bool, true si macOS ≥ 11
//   jamodio_audio_workgroup_join(name_substr)   → handle opaque ou NULL
//   jamodio_audio_workgroup_leave(handle)       → void
//
// `name_substr = NULL` ⇒ default output device.
//
// Le `os_workgroup_join` doit être appelé depuis le thread qui veut
// bénéficier du scheduling audio. Le wrapper Rust force ça via Drop +
// vérification d'ownership de thread (cf. wrappers `Send`/`!Sync`).
//
// Ref Apple :
//   https://developer.apple.com/documentation/audiotoolbox/workgroup_management
//   https://developer.apple.com/documentation/os/workgroup
//   WWDC 2020 « Optimize the Core Audio Workflow in Your App »

#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/CoreAudio.h>
#import <Foundation/Foundation.h>
#import <os/workgroup.h>
#import <stdbool.h>
#import <stdlib.h>
#import <string.h>
#include <new> // std::nothrow

// Struct opaque côté Rust — c'est notre handle de leave.
// Sous ARC ObjC++, `os_workgroup_t` est __strong implicite : le scope de
// l'objet est tracké via le cycle de vie de la struct (new/delete C++).
// Pas de release manuel — assigner `nil` libère.
struct JamodioWorkgroupHandle {
    os_workgroup_t workgroup;       // ARC __strong (auto-retain au store)
    os_workgroup_join_token_s token; // POD, token rendu par os_workgroup_join
};

extern "C" {

bool jamodio_audio_workgroup_available(void) {
    if (@available(macOS 11.0, *)) {
        return true;
    }
    return false;
}

// Helper : retourne l'AudioDeviceID du default output, ou kAudioObjectUnknown.
static AudioDeviceID jamodio_default_output_device(void) {
    AudioObjectPropertyAddress addr = {
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    AudioDeviceID dev = kAudioObjectUnknown;
    UInt32 size = sizeof(dev);
    OSStatus st = AudioObjectGetPropertyData(
        kAudioObjectSystemObject, &addr, 0, NULL, &size, &dev);
    if (st != noErr) {
        return kAudioObjectUnknown;
    }
    return dev;
}

// Helper : cherche un device output dont le nom CFString contient `substr`.
// Retourne kAudioObjectUnknown si rien ne match.
static AudioDeviceID jamodio_find_output_by_name(const char* substr) {
    if (substr == NULL || substr[0] == '\0') {
        return jamodio_default_output_device();
    }

    AudioObjectPropertyAddress addr = {
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject, &addr, 0, NULL, &size) != noErr) {
        return kAudioObjectUnknown;
    }
    if (size == 0) {
        return kAudioObjectUnknown;
    }

    UInt32 count = size / sizeof(AudioDeviceID);
    AudioDeviceID* ids = (AudioDeviceID*)malloc(size);
    if (ids == NULL) {
        return kAudioObjectUnknown;
    }
    if (AudioObjectGetPropertyData(
            kAudioObjectSystemObject, &addr, 0, NULL, &size, ids) != noErr) {
        free(ids);
        return kAudioObjectUnknown;
    }

    AudioDeviceID found = kAudioObjectUnknown;
    AudioObjectPropertyAddress name_addr = {
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    AudioObjectPropertyAddress streams_addr = {
        kAudioDevicePropertyStreams,
        kAudioDevicePropertyScopeOutput,
        kAudioObjectPropertyElementMain,
    };

    for (UInt32 i = 0; i < count; i++) {
        // Filtre : on ne considère que les devices avec au moins 1 stream OUTPUT.
        // Sans ça on peut matcher un device d'entrée seul (mic interne, BlackHole
        // input-only) qui n'a pas de workgroup pertinent pour notre playback.
        UInt32 streams_size = 0;
        if (AudioObjectGetPropertyDataSize(
                ids[i], &streams_addr, 0, NULL, &streams_size) != noErr) {
            continue;
        }
        if (streams_size == 0) {
            continue;
        }

        CFStringRef cf_name = NULL;
        UInt32 name_size = sizeof(cf_name);
        if (AudioObjectGetPropertyData(
                ids[i], &name_addr, 0, NULL, &name_size, &cf_name) != noErr) {
            continue;
        }
        if (cf_name == NULL) {
            continue;
        }

        char name_buf[256];
        bool got = CFStringGetCString(
            cf_name, name_buf, sizeof(name_buf), kCFStringEncodingUTF8);
        CFRelease(cf_name);
        if (!got) {
            continue;
        }
        // Match case-insensitive sur le substr (les noms Apple sont stables mais
        // les noms USB peuvent varier en casse selon le driver).
        if (strcasestr(name_buf, substr) != NULL) {
            found = ids[i];
            break;
        }
    }

    free(ids);
    // Fallback : si match nominal a échoué, on prend le default — toujours mieux
    // que NULL (au pire, on se schedule sur le HAL du speaker interne, qui est
    // quand même mieux que SCHED_OTHER).
    if (found == kAudioObjectUnknown) {
        found = jamodio_default_output_device();
    }
    return found;
}

void* jamodio_audio_workgroup_join(const char* device_name_substr) {
    if (@available(macOS 11.0, *)) {
        AudioDeviceID dev = jamodio_find_output_by_name(device_name_substr);
        if (dev == kAudioObjectUnknown) {
            return NULL;
        }

        AudioObjectPropertyAddress wg_addr = {
            kAudioDevicePropertyIOThreadOSWorkgroup,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        };
        os_workgroup_t wg = NULL;
        UInt32 wg_size = sizeof(wg);
        OSStatus status = AudioObjectGetPropertyData(
            dev, &wg_addr, 0, NULL, &wg_size, &wg);
        if (status != noErr || wg == NULL) {
            // Certains devices virtuels (BlackHole, Aggregate Device) n'exposent
            // pas de workgroup. Le caller fera son fallback (QoS).
            return NULL;
        }

        // `new` C++ ARC-aware : les champs __strong (workgroup) sont
        // correctement initialisés et seront release au `delete h` du leave.
        JamodioWorkgroupHandle* h = new (std::nothrow) JamodioWorkgroupHandle{};
        if (h == NULL) {
            // wg est __strong local → ARC release en sortie de scope.
            return NULL;
        }

        // os_workgroup_join : le caller (= le thread courant) entre dans le
        // workgroup HAL audio. Retourne 0 si succès, non-zero sinon (typique :
        // EALREADY = thread déjà dans un workgroup).
        int join_status = os_workgroup_join(wg, &h->token);
        if (join_status != 0) {
            delete h; // ARC release des champs __strong via le destructeur
            return NULL;
        }
        h->workgroup = wg; // ARC retain (assignation __strong)
        return (void*)h;
    }
    return NULL;
}

void jamodio_audio_workgroup_leave(void* handle) {
    if (handle == NULL) {
        return;
    }
    if (@available(macOS 11.0, *)) {
        JamodioWorkgroupHandle* h = (JamodioWorkgroupHandle*)handle;
        if (h->workgroup != nil) {
            os_workgroup_leave(h->workgroup, &h->token);
            // ARC release via `delete h` ci-dessous (champ __strong libéré
            // par le destructeur de la struct).
        }
        delete h;
    }
}

} // extern "C"
