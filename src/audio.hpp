#pragma once
#include <windows.h>
#include <mmdeviceapi.h>
#include <array>
#include <atomic>
#include <algorithm>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <mutex>
#include <unordered_map>
#include <string>
#include <thread>
#include <vector>
#include "hotkeys.hpp"

namespace mic {
constexpr unsigned rate = 48000, block = 480;
struct RvcSettings {
    unsigned slot=0, chunkMs=200, index=0, gain=100;
    int pitch=0;
    uint64_t packed() const {return slot | (uint64_t(pitch+24)<<16) | (uint64_t(index)<<22) | (uint64_t(chunkMs)<<29) | (uint64_t(gain)<<39);}
    static RvcSettings unpack(uint64_t v) {return {unsigned(v&65535),unsigned((v>>29)&1023),unsigned((v>>22)&127),unsigned((v>>39)&511),int((v>>16)&63)-24};}
};
struct Device { std::wstring id, name; };
inline int preferredDevice(const std::vector<Device>& list,const std::wstring& saved,const std::wstring& hint) {
    for(size_t i=0;i<list.size();++i)
        if(saved.empty()?list[i].name.find(hint)!=std::wstring::npos:list[i].id==saved) return static_cast<int>(i);
    return -1; // A missing saved device must never silently select a different microphone.
}
std::vector<Device> devices(bool capture,DWORD states=DEVICE_STATE_ACTIVE);
Device tagMicrophone();
void setEndpointProperty(IMMDevice* endpoint,REFPROPERTYKEY key,PROPVARIANT& value);
std::filesystem::path projectRoot();
std::string utf8(const std::wstring& s);
std::wstring wide(const std::string& s);
// Live process environment. The CRT's _wgetenv keeps a snapshot from its first call and misses
// later std::env::set_var changes made by the Rust UI.
std::wstring environment(const wchar_t* name);
// CPU denoiser supplied by the host program (DeepFilterNet in the Rust UI), used when NVIDIA
// cannot run. 480-sample 48 kHz mono frames; `process` returns 0 on failure. The native checks
// link without it and fall back to no denoiser.
struct CpuDenoiserApi {
    void* (*create)();
    int32_t (*process)(void* state,const float* in,float* out,float strength);
    void (*destroy)(void* state);
};
void setCpuDenoiser(const CpuDenoiserApi* api);
// NVIDIA model folder for CUDA device 0 (turing/ampere/ada/blackwell); `name` gets the GPU name.
std::string gpuArch(std::string& name);

// Single producer / single consumer. Only the consumer may discard old samples.
template<size_t Capacity,class T=float> class Ring {
    std::array<T, Capacity> data_{};
    alignas(64) std::atomic<uint64_t> head_{0};
    alignas(64) std::atomic<uint64_t> tail_{0};
public:
    size_t size() const {
        const auto t = tail_.load(std::memory_order_acquire);
        return static_cast<size_t>(head_.load(std::memory_order_acquire) - t);
    }
    bool push(const T* p, size_t n) {
        const auto h = head_.load(std::memory_order_relaxed);
        if(n > Capacity || h - tail_.load(std::memory_order_acquire) + n > Capacity) return false;
        for(size_t i = 0; i < n; ++i) data_[(h+i)%Capacity] = p[i];
        head_.store(h+n, std::memory_order_release);
        return true;
    }
    bool pop(T* p, size_t n) {
        const auto t = tail_.load(std::memory_order_relaxed);
        if(head_.load(std::memory_order_acquire) - t < n) return false;
        for(size_t i = 0; i < n; ++i) p[i] = data_[(t+i)%Capacity];
        tail_.store(t+n, std::memory_order_release);
        return true;
    }
    size_t trim(size_t keep) {
        const auto t = tail_.load(std::memory_order_relaxed);
        const auto available = head_.load(std::memory_order_acquire) - t;
        const auto drop = available > keep ? available - keep : 0;
        tail_.store(t+drop, std::memory_order_release);
        return static_cast<size_t>(drop);
    }
    void reset() { head_=0; tail_=0; } // Only after both threads have joined.
};

struct Drift {
    double filtered = 0, integral = 0;
    double update(double errorFrames) {
        filtered += 0.05 * (errorFrames-filtered);
        integral = std::clamp(integral + filtered*0.00000001, -0.0015, 0.0015);
        return std::clamp(integral + filtered*0.000003, -0.003, 0.003);
    }
};
struct Config {
    std::wstring input, output;
    int version = 1;
    float intensity = 1;
    unsigned bufferMs = 40;
    unsigned periodMs = 5;
    std::filesystem::path sdk;
    bool tag = false;
    std::filesystem::path tagSdk;
    int cudaGraphs = -1; // -1: SDK default, 0: disabled, 1: enabled.
};
void benchmarkAfx(const Config&, const std::vector<float>&, unsigned seconds, const std::filesystem::path& csv, bool churn = false);
struct TagClock {
    double pending = 0;
    unsigned take(double elapsed, double correction, unsigned maximum) {
        pending += elapsed*rate*(1+correction);
        const auto frames=static_cast<unsigned>(std::min(pending,static_cast<double>(maximum)));
        pending -= frames;
        return frames;
    }
};
struct Stats {
    std::atomic<int> desktopState{0}; // Off, Starting, Ready, Error
    std::atomic<int> denoiser{0}; // Stopped, NVIDIA, bypass, CPU (DeepFilterNet), input already denoised
    std::atomic<bool> desktopSource{false};
    std::atomic<int> phraseState{0};
    std::atomic<float> phraseSeconds{0};
    std::atomic<bool> pitchActive{false}, boostActive{false};
    std::atomic<float> pitchDelayMs{0}, pitchMaxMs{0};
    std::atomic<bool> outputActive{false};
    std::atomic<float> reconfigureMs{0};
    std::atomic<float> maxRunMs{0}, maxResetMs{0};
    // NVIDIA run time distribution: a lone max and a steady overload look the same otherwise.
    std::atomic<unsigned> runsOver5Ms{0}, runsOver10Ms{0}, maxRunBlock{0};
    std::atomic<float> inputPeak{0}, outputPeak{0}, processMs{0}, maxProcessMs{0};
    std::atomic<float> noisePeak{0}; // Cleaned microphone before the gate, independent of output effects.
    std::atomic<int> rvcState{0}; // Off, Starting, Ready, Bypass
    std::atomic<float> rvcLatencyMs{0};
    std::atomic<unsigned> inputQueue{0}, outputQueue{0}, renderPadding{0};
    std::atomic<unsigned> underruns{0}, drops{0}, discontinuities{0}, processed{0};
    std::atomic<float> inputPeriodMs{0}, outputPeriodMs{0}, driftPpm{0};
    std::atomic<unsigned> tagBufferFrames{0}, tagDriverGaps{0};
    std::atomic<uint64_t> tagFrames{0};
    std::atomic<unsigned> tagLateTicks{0}, tagReconnects{0};
    std::atomic<float> tagMaxWakeMs{0};
};
struct RoutedSample {float value=0;uint8_t discord=0,modified=0;unsigned epoch=0;float microphone=0;float sound=0;uint8_t recording=0;float echoMic=0,echoDiscord=0;};
// Soundpad clip: decoded by the UI to 48 kHz mono, owned here so playback never touches files.
struct SoundClip {
    std::vector<float> samples;
    std::atomic<float> gain{1};
    std::atomic<uint64_t> loopRange{0}; // first frame in high 32 bits, exclusive end in low 32 bits
    bool independentVolume=false;
};
constexpr unsigned recordingClipIdBase=900000;
constexpr uint64_t soundDoublePressMs=170;
// One clip at a time in the DSP thread: a request replaces, restarts (quick double press) or
// stops (same clip pressed again). Stop and replace ramp over 5 ms so Discord hears no click.
class SoundPlayer {
    std::shared_ptr<const SoundClip> clip_;
    size_t position_=0;
    unsigned id_=0;
    uint64_t serial_=0;
    float fade_=1,fadeStep_=0;
    size_t seekFrame_=0;bool seeking_=false,rendered_=false;
    std::shared_ptr<const SoundClip> next_;unsigned nextId_=0;
public:
    static uint64_t pack(unsigned id,uint64_t serial,bool restart){return (serial<<33)|(restart?1ull<<32:0)|id;}
    unsigned playing() const {return clip_?id_:0;}
    float position() const {return position_/48000.0f;}
    float length() const {return clip_?clip_->samples.size()/48000.0f:0;}
    // Returns the clip id to fetch from the library, or 0 when the request is consumed here.
    // A returned id is consumed only by commit(): a caller that cannot look it up yet retries.
    unsigned request(uint64_t packed) {
        const auto serial=packed>>33;if(serial==serial_)return 0;
        const unsigned id=static_cast<unsigned>(packed&0xffffffffu);const bool restart=(packed>>32)&1;
        if(id==0 || (!restart && clip_ && id==id_)){serial_=serial;stop();return 0;}
        return id;
    }
    void commit(uint64_t packed){serial_=packed>>33;}
    void stop(){seeking_=false;next_.reset();if(clip_&&fade_>0){fadeStep_=-1.0f/240;}}
    void start(unsigned id,std::shared_ptr<const SoundClip> clip){
        if(!clip||clip->samples.empty()){stop();return;}
        seeking_=false;
        if(clip_&&fade_>0){next_=std::move(clip);nextId_=id;fadeStep_=-1.0f/240;return;}
        clip_=std::move(clip);id_=id;position_=static_cast<unsigned>(clip_->loopRange.load()>>32);fade_=1;fadeStep_=0;rendered_=false;
    }
    bool seek(unsigned id,size_t frame){
        if(!clip_||id_!=id||next_)return false;
        const auto range=clip_->loopRange.load();
        const auto begin=static_cast<unsigned>(range>>32),end=static_cast<unsigned>(range);
        seekFrame_=end>begin?std::clamp(frame,size_t(begin),size_t(end-1)):std::min(frame,clip_->samples.size()-1);
        if(!rendered_){position_=seekFrame_;return true;} // nothing has been sent yet
        seeking_=true;fadeStep_=-1.0f/240;
        return true;
    }
    void render(float* out,unsigned count,float volume,bool muted=false,uint8_t* recording=nullptr) {
        for(unsigned i=0;i<count;++i){
            out[i]=0;
            if(recording)recording[i]=0;
            if(!clip_)continue;
            const auto range=clip_->loopRange.load(std::memory_order_relaxed);
            const auto begin=static_cast<unsigned>(range>>32),end=static_cast<unsigned>(range);
            const bool looping=end>begin && end<=clip_->samples.size();
            if(looping && position_<begin)position_=begin;
            if(looping && position_>=end)position_=begin;
            if(position_>=clip_->samples.size()){
                if(seeking_){position_=seekFrame_;seeking_=false;fade_=0;fadeStep_=1.0f/240;}
                else {clip_.reset();if(next_){start(nextId_,std::move(next_));next_.reset();}}
                continue;
            }
            if(recording)recording[i]=id_>=recordingClipIdBase;
            rendered_=true;
            const float playbackVolume=muted?0.0f:(id_>=recordingClipIdBase||clip_->independentVolume?1.0f:volume);
            float sample=clip_->samples[position_];
            if(looping){
                const size_t fade=std::min<size_t>(240,(end-begin)/2);
                const size_t edge=std::min(position_-begin+1,size_t(end)-position_);
                if(fade && edge<fade)sample*=static_cast<float>(edge)/static_cast<float>(fade);
            }
            ++position_;
            out[i]=std::clamp(sample*clip_->gain.load(std::memory_order_relaxed)*playbackVolume*fade_,-1.0f,1.0f);
            if(fadeStep_){
                fade_+=fadeStep_;
                if(fade_<=0){
                    fade_=0;
                    if(seeking_){position_=seekFrame_;seeking_=false;fadeStep_=1.0f/240;}
                    else {fadeStep_=0;clip_.reset();if(next_){start(nextId_,std::move(next_));next_.reset();}}
                } else if(fade_>=1.0f-1e-5f){fade_=1;fadeStep_=0;}
            }
        }
    }
};
// RVC audio crosses the worker boundary tagged with a generation: both rings are aligned
// streams of the same sample index space, so a new generation starts at index 0 on both sides.
struct RvcSample {float sample=0;unsigned generation=0;};
// ponytail: fixed inference/scheduling budget; raise if rvcState 3 shows up in normal use.
constexpr unsigned rvcSlack=9600; // 200 ms
// Fixed-delay playout: input index j is heard at j+delay. Late audio is dropped, never replayed;
// missing audio is silence. Latency therefore never creeps and speech is never heard twice.
struct RvcPlayout {
    uint64_t pushed=0,consumed=0;
    unsigned generation=0;
    // Returns 1 while priming, 2 when converted audio was output, 3 on underrun (silence).
    template<class Ring> int process(Ring& ring,float* out,unsigned count,uint8_t* modified,unsigned delay,unsigned current) {
        if(current!=generation){generation=current;pushed=consumed=0;}
        const int64_t first=static_cast<int64_t>(pushed)-delay;
        pushed+=count;
        std::fill_n(out,count,0.0f);
        if(modified)std::fill_n(modified,count,static_cast<uint8_t>(ModifiedEffects)); // the whole RVC region is an effect; silence previews as 0
        if(first+static_cast<int64_t>(count)<=0)return 1;
        RvcSample s;
        auto next=[&]{while(ring.pop(&s,1))if(s.generation==current)return true;return false;};
        if(first>0){
            while(consumed<static_cast<uint64_t>(first) && next())++consumed;
            if(consumed<static_cast<uint64_t>(first))return 3;
        }
        for(unsigned i=first<0?static_cast<unsigned>(-first):0;i<count;++i){
            if(!next())return 3;
            out[i]=s.sample;++consumed;
        }
        return 2;
    }
};
inline float previewSample(const RoutedSample& sample,uint8_t mask,unsigned epoch,bool audible) {
    return audible && (sample.modified&mask) && sample.epoch==epoch?sample.value:0;
}
// Producer side of the effects-only monitor: what goes into the preview queue for one output
// sample. The consumer applies previewSample again, so a clip sample must carry its
// monitor category here (preview queue only; routed samples never do).
inline RoutedSample previewQueued(float effectOnly,uint8_t modified,unsigned sampleEpoch,float sound,bool recording,uint8_t mask,unsigned epoch,bool audible) {
    const bool clip=((mask&ModifiedSound) || (recording && (mask&ModifiedEffects))) && sound!=0;
    const RoutedSample sample{effectOnly,0,modified,sampleEpoch};
    return {previewSample(sample,mask,epoch,audible)+(clip&&audible?sound:0),0,static_cast<uint8_t>(modified|(clip?ModifiedSound|(recording?ModifiedEffects:0):0)),epoch};
}
class Engine {
    friend void checkTagStack();
    friend void checkDiscordCapture(unsigned seconds);
    friend class Monitor;
    HANDLE stop_ = nullptr, data_ = nullptr, ready_ = nullptr;
    HANDLE tagOwner_ = nullptr;
    Config config_;
    std::thread io_, dsp_, desktopThread_, tagLevelThread_;
    std::atomic<float> tagLevelCompensation_{1};
    // UTC FILETIME of the first failure; the session line is written later, on the next stop().
    std::atomic<uint64_t> failedAt_{0};
    Ring<16384> captured_, desktop_;
    Ring<16384,RoutedSample> cleaned_;
    Ring<16384,RoutedSample> preview_;
    std::atomic<uint8_t> previewMask_{0};
    void preview(const float* audio,const float* echo,const RoutedSample* routed,const uint8_t* modified,unsigned count);
    std::mutex soundMutex_,soundRequestMutex_;
    std::unordered_map<unsigned,std::shared_ptr<SoundClip>> sounds_;
    // Replaced or cleared clips a player may still hold. Only library calls free them, so the
    // DSP thread never drops the last reference to a sample buffer (5 minutes = 57.6 MB).
    std::vector<std::shared_ptr<const SoundClip>> retiredSounds_;
    // Newest finished hold-effect recording, for the UI to save as a file. The DSP thread
    // only try-locks this slot: a contended block publishes on the next one.
    std::mutex clipMutex_;
    std::vector<float> clip_;
    std::mutex studioMutex_;
    std::vector<float> studioClip_;
    uint64_t soundSerial_=0,soundPressTick_=0;unsigned soundPressId_=0;
    std::atomic<bool> resetEffect_{false}, running_{false};
    mutable std::mutex statusMutex_;
    std::wstring status_ = L"Stopped";
    std::wstring desktopMessage_,denoiserMessage_;
    void ioLoop(Config config);
    void tagLoop(Config config);
    void dspLoop(Config config);
    void desktopLoop();
    void fail(const std::exception& error);
    void status(std::wstring text);
public:
    Stats stats;
    std::atomic<uint64_t> operation{0};
    uint64_t beginOperation(){const auto value=++operation;SetEvent(stop_);releaseEffects();return value;}
    // One atomic message: millisecond timestamp (29 bits), epoch (16), eligibility + 18 holds (19).
    std::atomic<uint64_t> heldSample{0};
    std::atomic<uint64_t> noiseHeldSample{0};
    std::atomic<unsigned> effectEpoch{0};
    std::atomic<bool> desktopEnabled{false};
    std::wstring desktopMessage() const;
    std::wstring denoiserMessage() const;
    std::atomic<int> state{0}; // Stopped, Loading, WaitingClient, Running, Stopping, Error
    std::atomic<float> volume{1}, boost{3};
    std::atomic<bool> overload{false};
    std::atomic<float> discordVolume{0.08f};
    std::atomic<float> slowSpeed{0.7f},fastSpeed{1.5f};
    std::atomic<unsigned> phraseCancel{0};
    std::atomic<unsigned> replayRequest{0};
    std::atomic<int> pitch{-5};
    std::atomic<int> formant{0},tuneRoot{0},tuneScale{0};
    std::atomic<unsigned> echoDelayMs{500},echoRepeats{3},echoDecay{55},echoLevel{100};
    std::atomic<unsigned> stutterMs{120},tuneSpeedMs{80},tuneStrength{100};
    std::atomic<unsigned> effectActivity{0};
    // Soundpad: library writes happen off the DSP thread; the DSP thread only try-locks.
    std::atomic<uint64_t> soundRequest{0};
    std::atomic<uint64_t> soundSeekRequest{0};
    std::atomic<float> soundVolume{1};
    std::atomic<unsigned> soundPlaying{0};
    std::atomic<float> soundPosition{0},soundLength{0};
    void soundLoad(unsigned id,std::vector<float> samples,float gain,unsigned loopStart=0,unsigned loopEnd=0,bool independentVolume=false);
    bool soundGain(unsigned id,float gain);
    bool soundLoop(unsigned id,unsigned start,unsigned end);
    void soundClear();
    void soundPlay(unsigned id,bool forceRestart=false);
    void soundSeek(unsigned id,unsigned frame);
    std::shared_ptr<const SoundClip> soundClip(unsigned id);
    // Bumped once per published recording; 0 means nothing was recorded yet.
    std::atomic<unsigned> clipGeneration{0};
    // Samples of the published recording, copying at most `capacity` of them into `out`.
    unsigned clipCopy(float* out,unsigned capacity,unsigned* generation);
    std::atomic<bool> studioRecording{false};
    std::atomic<unsigned> studioGeneration{0};
    unsigned studioCopy(float* out,unsigned capacity,unsigned* generation);
    unsigned held() const {
        const auto sample=heldSample.load();
        return heldFlags(sample,effectEpoch.load(),GetTickCount64(),running_ && stats.outputActive && !muted);
    }
    void releaseEffects() { heldSample=0; noiseHeldSample=0; effectActivity=0; ++effectEpoch; }
    void reportError(const std::string& message);
    std::atomic<float> intensity{1};
    std::atomic<float> alternateIntensity{0.15f};
    std::atomic<float> noiseGateDb{-72};
    std::atomic<bool> rvcEnabled{false};
    std::atomic<uint64_t> rvcConfig{RvcSettings{}.packed()};
    std::atomic<bool> muted{false};
    // Test hook: the TAG output thread sleeps once for this many ms (simulated preemption).
    std::atomic<unsigned> testStallMs{0};
    Engine();
    ~Engine();
    Engine(const Engine&) = delete;
    Engine& operator=(const Engine&) = delete;
    void start(const Config& config,uint64_t expectedOperation=0);
    void stop();
    bool running() const { return running_; }
    std::wstring status() const;
};
void checkDiscordCapture(unsigned seconds);
void checkTagLevel();
void checkTagLevelWatch();
void checkRvc();
void checkRvcIdle();
struct StereoSample {float left=0,right=0; unsigned epoch=0;};
void checkHeadphones(const std::wstring& output,bool denoise);
class Headphones {
    HANDLE stop_=nullptr,data_=nullptr,ready_=nullptr;
    HANDLE owner_=nullptr;
    std::thread io_,dsp_;
    Ring<16384,StereoSample> captured_,cleaned_;
    std::atomic<unsigned> epoch_{0};
    mutable std::mutex mutex_;
    std::wstring message_;
    void fail(const std::exception&);
    void ioLoop(std::wstring output);
    void dspLoop(bool denoise);
public:
    std::atomic<int> state{0},pitch{0}; // Off, Loading, Ready, Error
    std::atomic<float> intensity{0.8f},volume{0.7f};
    std::atomic<bool> muted{false},reverse{false};
    std::atomic<unsigned> processed{0},drops{0};
    Headphones();
    ~Headphones();
    void start(const std::wstring& output,bool denoise);
    void stop();
    std::wstring message() const;
};
// Independent preview of the virtual microphone. It never waits in the DSP/output path.
class Monitor {
    Engine& engine_;
    HANDLE stop_=nullptr;
    std::thread thread_;
    mutable std::mutex mutex_;
    std::wstring message_;
public:
    std::atomic<int> state{0}; // Off, Starting, Listening, Error
    std::atomic<uint64_t> frames{0};
    std::atomic<float> renderedPeak{0}; // Held maximum; the reader exchanges it back to 0.
    explicit Monitor(Engine& engine);
    ~Monitor();
    void start(const std::wstring& route,uint8_t effectsMask=0);
    void stop();
    std::wstring message() const;
};
}
