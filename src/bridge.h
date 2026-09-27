#pragma once
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct Mnr Mnr;
typedef struct {
    int32_t state,muted,pitch_active,boost_active;
    float input_peak,output_peak,process_ms,queue_ms,pitch_delay_ms,pitch_max_ms;
    uint32_t underruns,drops,epoch,captured_key;
    int32_t rvc_state;
    float rvc_latency_ms;
} MnrSnapshot;
Mnr* mnr_create(char* error,uint32_t capacity);
void mnr_destroy(Mnr*);
int32_t mnr_start(Mnr*,const char* input,uint32_t input_len,const char* output,uint32_t output_len,
    int32_t version,uint32_t buffer,uint32_t period,int32_t graphs,float intensity,char* error,uint32_t capacity);
void mnr_stop(Mnr*);
int32_t mnr_headphones(Mnr*,int32_t enabled,const char* output,uint32_t length,int32_t denoise,char* error,uint32_t capacity);
void mnr_headphone_controls(Mnr*,float intensity,float volume,int32_t pitch,int32_t muted);
// Continuous grain reverse on the headphone line (0/1; other values ignored). +200 ms while on.
void mnr_headphone_reverse(Mnr*,int32_t enabled);
int32_t mnr_headphone_state(Mnr*,char* text,uint32_t capacity);
// mode: 0 off, 1 full voice, otherwise 1 + mask (1 other effects, 2 boost, 4 soundpad): 2..8.
int32_t mnr_monitor(Mnr*,int32_t mode,char* error,uint32_t capacity);
int32_t mnr_monitor_state(Mnr*,char* text,uint32_t capacity);
// Peak of what the monitor actually rendered since the previous call (0 when idle).
float mnr_monitor_peak(Mnr*);
void mnr_controls(Mnr*,float volume,float boost,int32_t pitch,float intensity,int32_t muted,float slow,float fast,int32_t overload,float discordVolume,int32_t rvcEnabled);
typedef struct {
    int32_t echo_delay_ms,echo_repeats,echo_decay,echo_level;
    int32_t stutter_ms,grain_ms,grain_scatter_ms,grain_pitch; // grain_* reserved for ABI compatibility.
    int32_t tune_root,tune_scale,tune_speed_ms,tune_strength,formant;
} MnrEffectOptions;
// Additive ABI: existing controls and snapshot layouts stay unchanged.
void mnr_effect_options(Mnr*,const MnrEffectOptions*);
uint32_t mnr_effect_activity(Mnr*);
void mnr_rvc_settings(Mnr*,uint32_t slot,int32_t pitch,uint32_t index,uint32_t chunk_ms,uint32_t gain);
int32_t mnr_phrase_state(Mnr*,float* seconds);
int32_t mnr_discord_state(Mnr*,char* text,uint32_t capacity,int32_t* active);
// Host-supplied CPU denoiser (see mic::CpuDenoiserApi): create, process(state,in,out,strength), destroy.
typedef struct {void* (*create)(void);int32_t (*process)(void*,const float*,float*,float);void (*destroy)(void*);} MnrCpuDenoiser;
void mnr_set_cpu_denoiser(const MnrCpuDenoiser* api);
// 0 stopped/starting, 1 NVIDIA, 2 no denoiser, 3 CPU DeepFilterNet, 4 input is RTX Voice/Broadcast
// (no own denoiser); `text` says why NVIDIA is off, or names that input for 4.
int32_t mnr_denoiser_state(Mnr*,char* text,uint32_t capacity);
void mnr_phrase_cancel(Mnr*);
void mnr_snapshot(Mnr*,MnrSnapshot*,char* error,uint32_t capacity,int32_t meters);
int32_t mnr_devices(int32_t capture,char* result,uint32_t capacity);
int32_t mnr_refresh_host(char* error,uint32_t capacity);
int32_t mnr_tag_stop_host(char* error,uint32_t capacity);
int32_t mnr_tag_repair_lines(char* error,uint32_t capacity);
int32_t mnr_tag_device_state(char* detail,uint32_t capacity);
int32_t mnr_tag_legacy_host(int32_t stop,char* error,uint32_t capacity);
int32_t mnr_tag_remove_task(char* error,uint32_t capacity);
int32_t mnr_tag_task_enabled(int32_t mode,char* error,uint32_t capacity);
// NVIDIA model architecture of CUDA device 0 and its name as "arch<TAB>name"; 0 with the reason.
int32_t mnr_gpu(char* text,uint32_t capacity);
uint64_t mnr_begin_operation(Mnr*);
int32_t mnr_start_generation(Mnr*,const char* input,uint32_t il,const char* output,uint32_t ol,int32_t version,uint32_t buffer,uint32_t period,int32_t graphs,float intensity,char* error,uint32_t capacity,uint64_t generation);
// mode -1 reads login preference; 0/1 configure it. Returns -1 on failure.
int32_t mnr_tag_autostart(int32_t mode,char* error,uint32_t capacity);
void mnr_tag_task_warning(char* error,uint32_t capacity);
void mnr_bindings(Mnr*,const uint32_t* keys,uint32_t count);
void mnr_alternate_intensity(Mnr*,float intensity);
void mnr_capture_key(Mnr*,int32_t enabled);
uint32_t mnr_events(Mnr*);
int32_t mnr_shell_start(Mnr*,char* error,uint32_t capacity);
void mnr_tray_hint(Mnr*);
int32_t mnr_replace_file(const char* from,uint32_t from_len,const char* to,uint32_t to_len);
void mnr_usage(uint64_t* cpu_100ns,uint64_t* working_set);
// Soundpad. Clips are 48 kHz mono float, at most 5 minutes; id 0 means "stop" everywhere.
int32_t mnr_sound_load(Mnr*,uint32_t id,const float* samples,uint32_t count,float gain);
// Studio clips bypass the soundpad volume and can loop between sample frames [start,end).
int32_t mnr_studio_load(Mnr*,uint32_t id,const float* samples,uint32_t count,float gain,uint32_t start,uint32_t end);
int32_t mnr_sound_loop(Mnr*,uint32_t id,uint32_t start,uint32_t end);
// Offline pitch shift: output has exactly count frames, preserving sample duration.
int32_t mnr_studio_pitch(const float* input,uint32_t count,float scale,float* output);
int32_t mnr_sound_gain(Mnr*,uint32_t id,float gain);
void mnr_sound_clear(Mnr*);
void mnr_sound_play(Mnr*,uint32_t id);
void mnr_sound_restart(Mnr*,uint32_t id);
// Seek the matching playing clip; a request sent immediately after play waits for its start.
int32_t mnr_sound_seek(Mnr*,uint32_t id,float seconds);
void mnr_sound_volume(Mnr*,float volume);
// Hotkeys for clips: keys use the effect binding encoding; an entry with id 0 is the stop key.
int32_t mnr_sound_bindings(Mnr*,const uint32_t* ids,const uint32_t* keys,uint32_t count);
// Returns the playing clip id (0 idle) and fills position/length in seconds.
uint32_t mnr_sound_state(Mnr*,float* position,float* length);
// Newest finished hold-effect recording (48 kHz mono float). Returns how many samples it has
// and reports its generation; `out` may be null to ask for the size only.
uint32_t mnr_last_clip(Mnr*,float* out,uint32_t capacity,uint32_t* generation);
int32_t mnr_studio_record(Mnr*,int32_t enabled);
int32_t mnr_studio_recording(Mnr*);
uint32_t mnr_studio_clip(Mnr*,float* out,uint32_t capacity,uint32_t* generation);
// Modal picker on the calling thread: mode 0 folder, 1 audio files (multi-select).
// Writes newline-separated UTF-8 paths; returns 1, 0 when cancelled, -1 on error.
int32_t mnr_pick_paths(int32_t mode,char* result,uint32_t capacity);
#ifdef __cplusplus
}
#endif
