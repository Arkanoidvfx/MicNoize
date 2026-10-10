#pragma once
#include <rubberband/RubberBandLiveShifter.h>
#include <algorithm>
#include <cmath>
#include <vector>
#include <stdexcept>
#include <array>
#include <cstdint>
#include "hotkeys.hpp"

namespace mic {
// Fixed 10 ms transitions at 48 kHz. No lookahead in the gain stage.
struct Ramp {
    float value=0, target=0, step=0;
    unsigned left=0;
    explicit Ramp(float initial=0):value(initial),target(initial){}
    float next(float goal) {
        if(goal!=target) { target=goal; left=480; step=(target-value)/480; }
        if(left && !--left) value=target;
        else if(left) value+=step;
        return value;
    }
};
// Capture downmix to mono. Some inputs (Realtek jacks, two-capsule arrays) deliver channel 2
// phase-inverted: in stereo they sound right, averaged to mono the voice cancels and only the
// uncorrelated noise survives, which any denoiser (ours or RTX Voice) turns into a robotic voice.
// Channel 2 takes channel 1's polarity, decided from the energy-weighted correlation over about
// half a second with hysteresis. No allocation; one decision per packet.
struct Downmix {
    double xy=0, xx=0, yy=0;
    float sign=1;
    void process(const float* in,unsigned channels,unsigned n,bool silent,float* out) {
        if(channels>=2 && !silent && n) {
            double pxy=0, pxx=0, pyy=0;
            for(unsigned i=0;i<n;++i) {const double l=in[i*channels], r=in[i*channels+1]; pxy+=l*r; pxx+=l*l; pyy+=r*r;}
            if(std::isfinite(pxy+pxx+pyy)) {
                const double keep=std::pow(0.98,n/480.0);
                xy=xy*keep+pxy; xx=xx*keep+pxx; yy=yy*keep+pyy;
            }
            const double norm=std::sqrt(xx*yy);
            if(norm>1e-9) {const double c=xy/norm; if(c<-0.5) sign=-1; else if(c>0.5) sign=1;}
        }
        for(unsigned i=0;i<n;++i) {
            float v=0;
            if(!silent) for(unsigned ch=0;ch<channels;++ch) v+=(ch==1?sign:1.0f)*in[i*channels+ch];
            v/=static_cast<float>(channels);
            out[i]=std::isfinite(v)?std::clamp(v,-1.0f,1.0f):0;
        }
    }
};
// Microphone gate: 3 dB hysteresis, 120 ms hold and the shared 10 ms gain ramp.
// No lookahead, allocation or extra buffering. -72 dB is exact dry bypass.
struct NoiseGate {
    Ramp gain{0};
    size_t hold=0;
    float process(float* data,size_t n,float db,float* envelope=nullptr) {
        float peak=0;
        for(size_t i=0;i<n;++i)peak=std::max(peak,std::abs(data[i]));
        if(!std::isfinite(db) || db<=-72) {
            gain=Ramp{1};hold=0;
            if(envelope)std::fill_n(envelope,n,1.0f);
            return peak;
        }
        const float threshold=std::pow(10.0f,std::min(db,0.0f)/20.0f);
        const bool detected=peak>=threshold || (hold && peak>=threshold*0.70794578f);
        if(detected)hold=5760;
        const bool open=hold!=0;
        if(!detected)hold=hold>n?hold-n:0;
        for(size_t i=0;i<n;++i){
            const float level=gain.next(open?1.0f:0.0f);
            if(envelope)envelope[i]=level;
            data[i]*=level;
        }
        return peak;
    }
    // Hotkey playback keeps its samples; converted live voice still passes the gate.
    static void applyVoice(float* data,size_t n,const float* envelope,uint8_t* modified,bool discord) {
        for(size_t i=0;i<n;++i){
            if(!discord && !(modified[i]&~ModifiedVoice))data[i]*=envelope[i];
            if(modified[i]&ModifiedVoice)modified[i]=static_cast<uint8_t>((modified[i]&~ModifiedVoice)|ModifiedEffects);
        }
    }
};
struct OutputEffects {
    Ramp gain{1},boost{3},wet{0},drive{0},discordGain{0.08f};
    void process(float* data,size_t n,float volume,float multiplier,bool held,bool overload=false,const uint8_t* discord=nullptr,float discordVolume=0.08f,uint8_t* modified=nullptr,const float* microphone=nullptr,float* effectOnly=nullptr,const float* sound=nullptr,const float* echoMic=nullptr,const float* echoDiscord=nullptr,float* echoOnly=nullptr) {
        const bool enabled=held && multiplier>1;
        for(size_t i=0;i<n;++i) {
            const float x=(std::isfinite(data[i])?data[i]:0)*gain.next(volume);
            const float b=boost.next(multiplier),mix=wet.next(enabled?1.0f:0.0f);
            if(modified && mix>0)modified[i]|=ModifiedBoost;
            const float harsh=drive.next(overload?1.0f:0.0f);
            const float sourceGain=discordGain.next(discordVolume);
            float effect=x;
            if(mix!=0){
                const float soft=0.891f*std::tanh(b*x/0.891f);
                const float distorted=std::lerp(soft,std::clamp(12*b*x,-0.891f,0.891f),harsh);
                effect=x+mix*(distorted-x);
            }
            data[i]=std::clamp(effect,-1.0f,1.0f)*(discord && discord[i]?sourceGain:1.0f);
            if(effectOnly)effectOnly[i]=data[i];
            const float echo=((echoMic?echoMic[i]:0)+(echoDiscord?echoDiscord[i]*sourceGain:0))*gain.value;
            if(echoOnly)echoOnly[i]=echo;
            data[i]=std::clamp(data[i]+echo,-1.0f,1.0f);
            // Soundpad and the background microphone bypass effects and Discord gain.
            if(sound)data[i]=std::clamp(data[i]+sound[i],-1.0f,1.0f);
            if(microphone)data[i]=std::clamp(data[i]+microphone[i]*gain.value,-1.0f,1.0f);
        }
    }
};
struct SourceRouting {
    unsigned previous=0;
    bool phraseDiscord=false;
    unsigned phraseFlags=0;
    bool select(unsigned held,bool phraseActive) {
        const unsigned phrases=held&(HoldPhrases|(HoldPhrases<<DiscordShift));
        const bool conflict=(phrases&(phrases-1))!=0;
        if(phrases && !conflict && phrases!=previous)phraseDiscord=(phrases&(HoldPhrases<<DiscordShift))!=0;
        if(!phrases && !phraseActive)phraseDiscord=false;
        previous=phrases;
        phraseFlags=conflict?HoldPhrases:((held|(held>>DiscordShift))&HoldPhrases);
        return (phrases||phraseActive)?phraseDiscord:(held&(((HoldBoost|HoldPitch)<<DiscordShift)|(HoldNew<<4)))!=0;
    }
};
// One bounded, in-memory slot shared by all hold effects (20 s covers 10 s at x0.5).
class LastEffect {
    std::vector<float> audio_=std::vector<float>(48000*20);
    std::vector<uint8_t> modified_=std::vector<uint8_t>(48000*20);
    size_t count_=0,position_=0;
    unsigned epoch_=0,cancel_=0,request_=0,previous_=0;
    bool capturing_=false,playing_=false,discord_=false,finished_=false;
public:
    // Boost only changes gain: it neither starts a recording nor lands in one.
    static constexpr unsigned recordable=HoldAllMask&~(HoldBoost|(HoldBoost<<DiscordShift));
    // A capture that just ended, for the shell to publish; reading it clears the flag.
    bool finished() {const bool was=finished_;finished_=false;return was;}
    size_t count() const {return count_;}
    bool capturing() const {return capturing_;}
    bool recordingDiscord() const{return discord_;}
    void copyRecording(std::vector<float>& out,float discordVolume) const {
        out.assign(audio_.begin(),audio_.begin()+count_);
        if(discord_)for(auto& sample:out)sample*=discordVolume;
    }
    bool process(float* data,size_t n,uint8_t* modified,bool& discord,
                 unsigned allHeld,bool phraseActive,bool valid,unsigned epoch,unsigned cancel,unsigned request,
                 const float* capture=nullptr,const uint8_t* captureFlags=nullptr,bool tailActive=false) {
        const unsigned held=allHeld&recordable;
        if(epoch!=epoch_ || cancel!=cancel_ || !valid){
            if(capturing_)count_=0;
            capturing_=playing_=false;previous_=0;epoch_=epoch;cancel_=cancel;request_=request;
            return false;
        }
        const bool active=held || phraseActive || tailActive;
        if(held && held!=previous_){count_=0;capturing_=true;playing_=false;discord_=discord;}
        previous_=held;
        if(capturing_){
            for(size_t i=0;i<n;++i)if(((captureFlags?captureFlags[i]:modified[i])&~ModifiedBoost) && count_<audio_.size()){
                modified_[count_]=static_cast<uint8_t>((captureFlags?captureFlags[i]:modified[i])&~ModifiedBoost);
                audio_[count_++]=capture?capture[i]:data[i];
            }
            if(!active){capturing_=false;finished_=count_>0;}
        }
        if(request!=request_){
            request_=request;
            if(!active && !capturing_ && count_){position_=0;playing_=true;}
        }
        if(!playing_)return false;
        discord=discord_;
        for(size_t i=0;i<n;++i){
            // The live microphone accompanies replay, but is never saved into a Discord clip.
            if(position_<count_){modified[i]=modified_[position_];data[i]=audio_[position_++];}
            else {data[i]=0;modified[i]=0;}
        }
        playing_=position_<count_;
        return true;
    }
};
// A bounded phrase recorder: tape speed or live speech followed by delayed reverse.
// 32-tap windowed sinc prevents aliasing on acceleration; the live bypass is exact.
class PhraseEffect {
public:
    // Numeric values are the C ABI contract (mnr_phrase_state) and the Rust UI's match arms.
    enum State : int {
        Idle=0, RecordSlow=1, RecordFast=2, PlaySlow=3, PlayFast=4, TailSlow=5, TailFast=6,
        FullSlow=7, FullFast=8, RecordReverse=9, ReversePause=10, FullReverse=11, PlayReverse=12
    };
private:
    static constexpr size_t limit=48000*10, phases=256, taps=32;
    std::vector<float> audio_=std::vector<float>(limit);
    std::array<std::array<float,taps>,phases> filter_{};
    size_t count_=0,tail_=0,played_=0;
    std::array<float,limit/480> reversePeaks_{};
    size_t reverseBegin_=0,reverseEnd_=0;
    double position_=0,speed_=1;
    unsigned mode_=0,lastHeld_=0,epoch_=0,cancel_=0;
    bool blocked_=false;
    int state_=Idle;
    Ramp live_{1};
    bool reverse() const{return mode_==HoldReverse;}
    bool playing() const{return state_==PlaySlow||state_==PlayFast||state_==PlayReverse;}
    void clear(){count_=tail_=played_=reverseBegin_=reverseEnd_=0;position_=0;mode_=0;state_=Idle;}
    void trimReverse() {
        reverseBegin_=0;reverseEnd_=count_;
        if(count_<14400)return;
        const size_t windows=(count_+479)/480;
        const float peak=*std::max_element(reversePeaks_.begin(),reversePeaks_.begin()+windows);
        if(peak<=0.001f)return;
        // ponytail: only near-silence; noisy edges intentionally stay intact.
        const float threshold=std::min(0.0001f,peak*0.001f);
        size_t first=0,last=windows;
        while(first<windows && reversePeaks_[first]<threshold)++first;
        while(last>first && reversePeaks_[last-1]<threshold)--last;
        if(first==last)return;
        if(first*480>=9600)reverseBegin_=first*480-4800;
        if(last*480<=count_ && count_-last*480>=9600)reverseEnd_=last*480+4800;
        if(reverseBegin_>=reverseEnd_ || reverseEnd_>count_){reverseBegin_=0;reverseEnd_=count_;}
    }
    size_t playbackSize() const{return reverse()?reverseEnd_-reverseBegin_:count_;}
    void begin(unsigned mode,float speed) {
        clear();mode_=mode;speed_=speed;state_=mode==HoldReverse?RecordReverse:(mode==HoldSlow?RecordSlow:RecordFast);
        if(reverse())return; // Reverse uses exact recorded samples at normal speed.
        const double cutoff=std::min(1.0,1.0/speed_)*0.94;
        for(size_t p=0;p<phases;++p){
            double sum=0;
            for(size_t j=0;j<taps;++j){
                const double x=static_cast<double>(j)-15-static_cast<double>(p)/phases;
                const double a=3.141592653589793*x*cutoff;
                const double v=cutoff*(std::abs(a)<1e-9?1:std::sin(a)/a)*(0.5+0.5*std::cos(3.141592653589793*x/16));
                filter_[p][j]=static_cast<float>(v);sum+=v;
            }
            for(auto& v:filter_[p])v=static_cast<float>(v/sum);
        }
    }
    bool recording() const{return state_!=Idle && !playing() && state_!=ReversePause;}
public:
    int state() const{return state_;}
    float seconds() const{return static_cast<float>(state_==ReversePause?tail_:(playing()?(playbackSize()-position_)/speed_:count_))/48000;}
    void process(float* data,size_t n,unsigned held,float slow,float fast,bool valid,unsigned epoch,unsigned cancel,bool reverseLive=true,uint8_t* modified=nullptr) {
        const unsigned mode=held&HoldPhrases;
        const bool conflict=(mode&(mode-1))!=0;
        if(epoch!=epoch_ || cancel!=cancel_ || !valid || conflict){
            clear();blocked_=mode!=0;epoch_=epoch;cancel_=cancel;
        }
        if(!mode)blocked_=false;
        if(valid && !blocked_ && mode && mode!=lastHeld_ && !conflict)
            begin(mode,mode==HoldReverse?1.0f:std::clamp(mode==HoldSlow?slow:fast,mode==HoldSlow?0.5f:1.05f,mode==HoldSlow?0.95f:2.0f));
        if(valid && recording() && !mode && lastHeld_){
            if(reverse())trimReverse();
            tail_=reverse()?7200:std::min<size_t>(9600,limit-count_);
            state_=reverse()?ReversePause:(mode_==HoldSlow?TailSlow:TailFast);
        }
        lastHeld_=mode;
        if(state_==Idle && live_.value==1)return; // No sample work or added delay in the normal live path.
        for(size_t i=0;i<n;++i){
            const float dry=std::isfinite(data[i])?data[i]:0;
            if(state_==ReversePause){
                if(modified)modified[i]=0;
                data[i]=0;live_.next(0);
                if(!--tail_){state_=PlayReverse;position_=0;played_=0;if(!playbackSize())clear();}
                continue;
            }
            if(recording()) {
                if(count_<limit){
                    if(reverse()){
                        auto& peak=reversePeaks_[count_/480];
                        if(count_%480==0)peak=0;
                        peak=std::max(peak,std::abs(dry));
                    }
                    audio_[count_++]=dry;
                }
                if(state_==TailSlow||state_==TailFast){
                    if(tail_)--tail_;
                    if(!tail_){state_=mode_==HoldSlow?PlaySlow:PlayFast;position_=0;played_=0;}
                }else if(count_==limit)state_=reverse()?FullReverse:(mode_==HoldSlow?FullSlow:FullFast);
                const float live=live_.next(reverseLive?1.0f:0.0f);
                if(modified)modified[i]=0;
                data[i]=reverseLive?dry*live:0;continue;
            }
            if(playing()){
                if(modified)modified[i]=ModifiedEffects;
                const auto center=static_cast<int64_t>(position_);
                const auto phase=std::min(phases-1,static_cast<size_t>((position_-center)*phases));
                double v=0;
                const size_t length=playbackSize();
                if(reverse()) v=audio_[reverseEnd_-1-played_];
                else for(size_t j=0;j<taps;++j){const auto at=center+static_cast<int64_t>(j)-15;if(at>=0&&static_cast<size_t>(at)<count_)v+=audio_[static_cast<size_t>(at)]*filter_[phase][j];}
                double fade=std::min({1.0,played_/480.0,(count_-position_)/(speed_*480)});
                if(reverse()){
                    // Ease the reverse ending to exact silence over 60 ms (half of a very short phrase).
                    const double end=std::clamp((length-position_-1)/std::min(2880.0,length/2.0),0.0,1.0);
                    fade=std::min(played_/480.0,end*end*(3-2*end));
                }
                data[i]=std::clamp(static_cast<float>(v*fade),-1.0f,1.0f);
                position_+=speed_;++played_;
                if(position_>=length)clear();
            }else data[i]=dry*live_.next(1);
        }
    }
};
// Continuous reverse as close to live as reverse can be: every 100 ms the last 200 ms are
// played backwards under a periodic Hann window; at 50 % overlap the windows sum to 1, so
// joints neither click nor pump. Latency is one grain (200 ms) only while it is on.
// Buffers are allocated once, before the audio loop; process() never allocates.
class GrainReverse {
public:
    static constexpr unsigned Grain=9600,Hop=Grain/2;
    GrainReverse():history_(Grain),output_(Grain),window_(Grain) {
        for(unsigned k=0;k<Grain;++k) window_[k]=0.5f-0.5f*std::cos(6.28318530718f*k/Grain);
    }
    void reset(){std::fill(history_.begin(),history_.end(),0.f);std::fill(output_.begin(),output_.end(),0.f);write_=read_=count_=0;}
    void process(float* data,unsigned frames) {
        for(unsigned i=0;i<frames;++i) {
            history_[write_]=data[i];write_=(write_+1)%Grain;
            data[i]=output_[read_];output_[read_]=0;read_=(read_+1)%Grain;
            if(++count_==Hop) {
                count_=0;
                // Newest sample first: the grain comes out reversed, starting at the read head.
                for(unsigned k=0;k<Grain;++k) output_[(read_+k)%Grain]+=history_[(write_+Grain-1-k)%Grain]*window_[k];
            }
        }
    }
private:
    std::vector<float> history_,output_,window_;
    unsigned write_=0,read_=0,count_=0;
};
// Capture the completed history preceding a press. History is fed separately for each source.
class StutterEffect {
    std::array<float,14400> history_{},loop_{};
    size_t write_=0,filled_=0,length_=0,position_=0;
    bool wasHeld_=false;
    Ramp wet_;
public:
    void reset(){write_=filled_=length_=position_=0;wasHeld_=false;wet_=Ramp{};}
    void feed(const float* data,size_t n){
        for(size_t i=0;i<n;++i){history_[write_]=std::isfinite(data[i])?data[i]:0;write_=(write_+1)%history_.size();filled_=std::min(filled_+1,history_.size());}
    }
    void process(float* data,size_t n,bool held,unsigned milliseconds,uint8_t* modified=nullptr){
        if(held&&!wasHeld_){
            length_=std::min(filled_,static_cast<size_t>(std::clamp(milliseconds,50u,300u))*48);
            for(size_t i=0;i<length_;++i)loop_[i]=history_[(write_+history_.size()-length_+i)%history_.size()];
            position_=0;
        }
        wasHeld_=held;
        for(size_t i=0;i<n;++i){
            const float mix=wet_.next(held&&length_?1.0f:0.0f);
            if(mix>0 && length_){
                const float edge=std::min({1.0f,position_/240.0f,(length_-position_-1)/240.0f});
                data[i]=std::clamp(std::lerp(data[i],loop_[position_]*edge,mix),-1.0f,1.0f);
                if(modified)modified[i]|=ModifiedEffects;
                position_=(position_+1)%length_;
            }
        }
        if(!held&&wet_.value==0)length_=0;
    }
};
// Finite taps from the captured input only. The output is never written back into the delay.
class EchoEffect {
    static constexpr size_t capacity=48000*16+1; // Eight 2 s taps, plus the sample being written.
    std::vector<float> history_=std::vector<float>(capacity);
    std::array<float,8> gains_{};
    size_t write_=0,age_=0,remaining_=0,delay_=10560;
    unsigned repeats_=3;
    bool wasHeld_=false;
public:
    void reset(){write_=age_=remaining_=0;wasHeld_=false;}
    bool active() const{return wasHeld_||remaining_>0;}
    void process(const float* dry,size_t n,bool held,unsigned delayMs,unsigned repeats,unsigned decay,unsigned level,float* wet){
        if(held&&!wasHeld_){
            reset();delay_=static_cast<size_t>(std::clamp(delayMs,60u,2000u))*48;
            repeats_=std::clamp(repeats,1u,8u);
            const float fall=std::clamp(decay,0u,90u)/100.0f;
            float gain=std::clamp(level,0u,100u)/100.0f;
            for(unsigned k=0;k<repeats_;++k){gains_[k]=gain;gain*=fall;}
        }
        if(!held&&wasHeld_)remaining_=delay_*repeats_;
        wasHeld_=held;
        for(size_t i=0;i<n;++i){
            wet[i]=0;
            if(!active())continue;
            history_[write_]=held&&dry?std::clamp(std::isfinite(dry[i])?dry[i]:0.0f,-1.0f,1.0f):0.0f;
            for(unsigned k=1;k<=repeats_;++k){
                const size_t lag=delay_*k;
                if(age_>=lag)wet[i]+=history_[(write_+capacity-lag)%capacity]*gains_[k-1];
            }
            if(!held){
                if(remaining_<480)wet[i]*=remaining_/480.0f;
                --remaining_;
            }
            wet[i]=std::clamp(wet[i],-1.0f,1.0f);
            write_=(write_+1)%capacity;age_=std::min(age_+1,capacity);
        }
    }
};
// 40 ms of 12 kHz samples, updated every 10 ms. A voiced YIN minimum controls one live shifter.
// Speed 0 snaps to the note at once (the robotic hard tune). A held note changes only when another
// scale note is clearly nearer, so it does not flicker between two notes; through short unvoiced gaps
// (consonants, breath) the last correction holds for 120 ms, then eases back to neutral.
class AutoTunePitch {
    static constexpr unsigned scales[4]={0xFFF,0xAB5,0x5AD,0x4A9}; // chromatic, major, minor, minor pentatonic
    std::array<float,480> history_{};
    size_t write_=0,filled_=0;
    float correction_=0;
    int target_=-1000;
    unsigned silentMs_=0;
    bool voiced_=false;
public:
    void reset(){write_=filled_=0;correction_=0;target_=-1000;silentMs_=0;voiced_=false;}
    bool voiced() const{return voiced_;}
    float process(const float* data,size_t n,bool held,int manual,int root,int scale,unsigned speed,unsigned strength){
        for(size_t i=0;i<n;i+=4){
            float sample=0;for(size_t j=i;j<std::min(i+4,n);++j)sample+=std::isfinite(data[j])?data[j]:0;
            history_[write_]=sample/4;write_=(write_+1)%history_.size();filled_=std::min(filled_+1,history_.size());
        }
        voiced_=false;
        if(!held){correction_=0;target_=-1000;silentMs_=0;return 0;}
        float desired=0;
        if(filled_==history_.size()){
            std::array<float,151> difference{},normalized{};
            double energy=0;std::array<double,4> quarter{};
            for(size_t i=0;i<history_.size();++i){const float v=history_[(write_+i)%history_.size()];energy+=v*v;quarter[i/120]+=v*v;}
            // A window that is partly silent (voice starting or stopping) gives a false period: skip it.
            const bool steady=*std::min_element(quarter.begin(),quarter.end())>0.1*energy/4;
            if(energy/history_.size()>0.000016 && steady){
                double running=0;int chosen=0;float best=1;
                for(int lag=15;lag<=150;++lag){
                    double d=0;
                    for(int i=0;i<480-lag;++i){const float a=history_[(write_+i)%480]-history_[(write_+i+lag)%480];d+=a*a;}
                    difference[lag]=static_cast<float>(d);running+=d;
                    normalized[lag]=static_cast<float>(d*lag/std::max(running,1e-12));
                    if(normalized[lag]<best){best=normalized[lag];chosen=lag;}
                }
                if(best<0.2f && chosen>0){
                    for(int lag=16;lag<150;++lag)if(normalized[lag]<0.15f && normalized[lag]<=normalized[lag-1] && normalized[lag]<normalized[lag+1]){chosen=lag;break;}
                    chosen=std::clamp(chosen,16,149);
                    const float left=difference[chosen-1],middle=difference[chosen],right=difference[chosen+1];
                    const float curve=left-2*middle+right;
                    const float offset=std::abs(curve)>1e-9f?std::clamp(0.5f*(left-right)/curve,-0.5f,0.5f):0;
                    const float midi=69+12*std::log2((12000.0f/(chosen+offset))/440.0f)+manual;
                    const unsigned mask=scales[std::clamp(scale,0,3)];
                    const auto allowed=[&](int note){return (mask>>static_cast<unsigned>((note-root%12+1200)%12)&1)!=0;};
                    const int center=static_cast<int>(std::round(midi));
                    int nearest=center;float distance=100;
                    for(int note=center-6;note<=center+6;++note)
                        if(allowed(note)&&std::abs(note-midi)<distance){distance=std::abs(note-midi);nearest=note;}
                    // Hysteresis: keep the held note until another is nearer by a quarter tone.
                    if(target_<-999||!allowed(target_)||std::abs(target_-midi)>distance+0.25f)target_=nearest;
                    desired=std::clamp(target_-midi,-4.0f,4.0f)*std::clamp(strength,0u,100u)/100.0f;
                    voiced_=true;
                }
            }
        }
        if(voiced_){
            silentMs_=0;
            correction_=speed==0?desired:correction_+(1-std::exp(-10.0f/std::min(speed,150u)))*(desired-correction_);
        }else if((silentMs_+=10)>120){
            // Keep the delayed stream continuous through consonants; return to neutral smoothly.
            target_=-1000;
            correction_+=(1-std::exp(-10.0f/60))*(0-correction_);
        }
        return correction_;
    }
};
class PitchEffect {
    RubberBand::RubberBandLiveShifter shifter_{48000,1,0};
    const size_t size_=shifter_.getBlockSize();
    std::vector<float> input_=std::vector<float>(size_),output_=std::vector<float>(size_);
    size_t in_=0,out_=0,total_=0,delay_=0;
    bool hasOutput_=false,started_=false;
    Ramp wet_;
    void begin(float semitones,float formants) {
        shifter_.reset(); shifter_.setPitchScale(std::exp2(semitones/12.0f));
        shifter_.setFormantScale(std::exp2(formants/12.0f));
        delay_=shifter_.getStartDelay();
        in_=out_=total_=0; hasOutput_=false; started_=true;
    }
public:
    void reset() { shifter_.reset();in_=out_=total_=delay_=0;hasOutput_=started_=false;wet_=Ramp{}; }
    bool active() const {return wet_.value>0;}
    float delayMs() const {return static_cast<float>(delay_+size_)*1000/48000;}
    void process(float* data,size_t n,int semitones,bool held,uint8_t* modified=nullptr) {
        processAdvanced(data,n,static_cast<float>(semitones),static_cast<float>(semitones),held&&semitones!=0,modified);
    }
    void processAdvanced(float* data,size_t n,float semitones,float formants,bool wanted,uint8_t* modified=nullptr) {
        if(wanted){
            if(!started_)begin(semitones,formants);
            shifter_.setPitchScale(std::exp2(semitones/12.0f));
            shifter_.setFormantScale(std::exp2(formants/12.0f));
        }
        for(size_t i=0;i<n;++i) {
            if(started_ && wet_.value==0 && !wanted) started_=false;
            if(!started_) continue; // Exact dry bypass, no pitch work or buffering.
            const float dry=data[i];
            const bool valid=hasOutput_ && total_>=delay_+size_;
            const float shifted=hasOutput_?output_[out_++]:0;
            if(out_==size_) hasOutput_=false;
            input_[in_++]=dry; ++total_;
            if(in_==size_) {
                const float* source=input_.data(); float* destination=output_.data();
                shifter_.shift(&source,&destination);
                in_=out_=0; hasOutput_=true;
            }
            const float mix=wet_.next(wanted && valid?1.0f:0.0f);
            if(modified && mix>0)modified[i]=ModifiedEffects;
            data[i]=mix==0?dry:dry+mix*(shifted-dry);
            if(!std::isfinite(data[i])) throw std::runtime_error("Pitch returned non-finite audio");
            data[i]=std::clamp(data[i],-1.0f,1.0f);
        }
    }
};
}
