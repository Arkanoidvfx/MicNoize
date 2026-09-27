#include "bridge.h"
#include "audio.hpp"
#include <iostream>
#include <limits>
#include <stdexcept>
#include <cstring>
#include <cmath>
#include <numbers>
static void require(bool ok,const char* message){if(!ok)throw std::runtime_error(message);}
namespace mic {
void checkTagStack() {
    // Leave 64 KiB of the shipped 1 MiB stack for callers/Windows audio APIs.
    // No microphone or host is opened; mic_check has a larger test stack.
    auto engine=std::make_unique<Engine>();
    HANDLE thread=CreateThread(nullptr,960*1024,[](void* context)->DWORD {
        const HRESULT initialized=CoInitializeEx(nullptr,COINIT_MULTITHREADED);
        if(FAILED(initialized))return 1;
        DWORD result=1;
        try {
            Config c;c.input=L"MicNoize-stack-check-invalid-endpoint";
            static_cast<Engine*>(context)->tagLoop(c);
        } catch(const std::exception& error) {
            result=std::strstr(error.what(),"Selected endpoint disconnected")?0:1;
        }
        CoUninitialize();return result;
    },engine.get(),STACK_SIZE_PARAM_IS_A_RESERVATION,nullptr);
    require(thread!=nullptr,"Create TAG stack check thread");
    WaitForSingleObject(thread,INFINITE);
    DWORD result=1;GetExitCodeThread(thread,&result);CloseHandle(thread);
    require(result==0,"TAG startup did not reject the invalid endpoint normally");
}
}
int main(){try{
    mic::checkTagStack();
    mic::checkRvcIdle();
    static_assert(sizeof(MnrSnapshot)==64);
    char error[4096]{};Mnr* p=mnr_create(error,sizeof(error));require(p!=nullptr,error);
    struct Guard{Mnr* p;~Guard(){mnr_destroy(p);}}guard{p};
    MnrSnapshot s{};mnr_snapshot(p,&s,error,sizeof(error),1);require(s.state==0,"Initial state");
    unsigned studioGeneration=99;
    require(!mnr_studio_record(p,1) && !mnr_studio_recording(p),"Stopped studio recorder started");
    require(mnr_studio_clip(p,nullptr,0,&studioGeneration)==0 && studioGeneration==0,"Empty studio clip state");
    require(mnr_monitor_state(p,error,sizeof(error))==0,"Monitor must start off");
    require(!mnr_monitor(p,1,error,sizeof(error)),"Stopped engine must not enable monitoring");
    require(mnr_monitor(p,0,error,sizeof(error))==1,"Monitor off must be idempotent");
    for(int mode:{2,3,4}){
        require(!mnr_monitor(p,mode,error,sizeof(error)),"Stopped engine enabled effect preview");
        require(strstr(error,"Start processing before listening")!=nullptr,"Effect monitor mode rejected at ABI");
    }
    require(!mnr_monitor(p,9,error,sizeof(error)) && strstr(error,"Invalid monitor mode"),"Invalid monitor mask accepted");
    require(!mnr_monitor(p,8,error,sizeof(error)) && strstr(error,"Start processing before listening"),"Soundpad monitor mask rejected at ABI");
    require(mnr_headphone_state(p,error,sizeof(error))==0,"Headphones must start off");
    require(mnr_headphones(p,0,"",0,0,error,sizeof(error))==1,"Headphone stop must be idempotent");
    require(mnr_headphones(p,2,"",0,0,error,sizeof(error))==0,"Invalid headphone mode accepted");
    require(mnr_headphones(p,1,"",40000,0,error,sizeof(error))==0,"Invalid headphone string accepted");
    mnr_controls(p,0.5f,3,-5,1,1,0.7f,1.5f,1,0.5f,1);mnr_snapshot(p,&s,error,sizeof(error),1);require(s.muted==1&&s.rvc_state==0,"Controls");
    const auto epoch=s.epoch;
    uint32_t keys[]={119,120,121,122,123,124,125,126,127,128,129,130};mnr_bindings(p,keys,12);mnr_snapshot(p,&s,error,sizeof(error),1);require(s.epoch!=epoch,"Bindings must reset holds");
    const auto bindingEpoch=s.epoch;keys[2]=119;mnr_bindings(p,keys,12);mnr_snapshot(p,&s,error,sizeof(error),1);require(s.epoch==bindingEpoch,"Duplicate phrase binding accepted");
    keys[2]=121;keys[9]=119;mnr_bindings(p,keys,12);mnr_snapshot(p,&s,error,sizeof(error),1);require(s.epoch==bindingEpoch,"Duplicate reverse binding accepted");
    uint32_t extended[]={119,120,121,122,123,124,125,126,127,128,129,130,131};
    mnr_bindings(p,extended,13);mnr_snapshot(p,&s,error,sizeof(error),1);
    require(s.epoch!=bindingEpoch,"Thirteenth noise binding rejected");
    const auto noiseEpoch=s.epoch;extended[12]=119;
    mnr_bindings(p,extended,13);mnr_snapshot(p,&s,error,sizeof(error),1);
    require(s.epoch==noiseEpoch,"Duplicate noise binding accepted");
    MnrEffectOptions options{220,3,55,100,120,80,30,0,0,0,80,100,0};
    mnr_effect_options(p,&options);options.echo_delay_ms=601;mnr_effect_options(p,&options);
    uint32_t allKeys[21]{};for(unsigned i=0;i<21;++i)allKeys[i]=119+i;
    mnr_bindings(p,allKeys,21);mnr_snapshot(p,&s,error,sizeof(error),1);
    require(s.epoch!=noiseEpoch,"New effect bindings rejected");
    const auto expandedEpoch=s.epoch;allKeys[20]=allKeys[13];mnr_bindings(p,allKeys,21);
    mnr_snapshot(p,&s,error,sizeof(error),1);require(s.epoch==expandedEpoch,"Duplicate new binding accepted");
    uint32_t clipId=42,clipKey=132;
    require(!mnr_sound_bindings(p,&clipId,&clipKey,1),"New effect key duplicated by soundpad");
    mnr_alternate_intensity(p,0.15f);
    {
        std::vector<float> tone(48000),shifted(tone.size());
        for(size_t i=0;i<tone.size();++i)tone[i]=0.3f*std::sin(2*std::numbers::pi*440.0*i/48000.0);
        require(!mnr_studio_pitch(tone.data(),tone.size(),0,shifted.data())
            && mnr_studio_pitch(tone.data(),tone.size(),2,shifted.data()),"Studio pitch shift failed");
        int crossings=0;double energy=0;
        for(size_t i=24001;i<28800;++i)if(shifted[i-1]<=0 && shifted[i]>0)++crossings;
        for(size_t i=38400;i<43200;++i)energy+=shifted[i]*shifted[i];
        require(crossings>75 && crossings<100 && energy>20,"Studio pitch changed duration or missed the octave");
        std::array<float,480> shortInput{},shortOutput{};
        shortInput.fill(0.3f);
        require(mnr_studio_pitch(shortInput.data(),shortInput.size(),2,shortOutput.data()),"Short studio pitch shift failed");
        require(std::abs(shortOutput[240])>0.05f,"Short studio sample became silent");
        std::vector<float> nearPad(8191,0.2f),nearPadOut(nearPad.size());
        require(mnr_studio_pitch(nearPad.data(),nearPad.size(),0.5f,nearPadOut.data())
            && std::isfinite(nearPadOut.back()),"Studio pitch padding overran the output");
    }
    {
        float clip[480];for(auto& v:clip)v=0.5f;float bad[1]{std::numeric_limits<float>::quiet_NaN()};
        require(mnr_sound_load(p,0,clip,480,1)==0 && mnr_sound_load(p,1,nullptr,480,1)==0 && mnr_sound_load(p,1,clip,0,1)==0,"Invalid clip accepted");
        require(mnr_sound_load(p,1,bad,1,1)==1 && mnr_sound_load(p,2,clip,480,1.5f)==1,"Clip load failed");
        require(!mnr_studio_load(p,800000,clip,480,1,400,200)
            && !mnr_studio_load(p,800000,clip,480,1,1,0)
            && mnr_studio_load(p,800000,clip,480,1,0,480)
            && !mnr_sound_loop(p,800000,400,200)
            && mnr_sound_loop(p,800000,0,240)
            && mnr_sound_loop(p,800000,0,0),"Studio loop validation");
        require(mnr_sound_gain(p,2,0.5f)==1 && mnr_sound_gain(p,9,0.5f)==0 && mnr_sound_gain(p,2,3)==0,"Clip gain validation");
        uint32_t ids[]={1,2,0};uint32_t soundKeys[]={200,201,202};
        require(mnr_sound_bindings(p,ids,soundKeys,3)==1,"Sound bindings rejected");
        soundKeys[1]=extended[0];require(mnr_sound_bindings(p,ids,soundKeys,3)==0,"Sound key colliding with an effect key accepted");
        soundKeys[1]=200;require(mnr_sound_bindings(p,ids,soundKeys,3)==0,"Duplicate sound key accepted");
        soundKeys[1]=0;require(mnr_sound_bindings(p,ids,soundKeys,3)==0,"Empty sound key accepted");
        require(mnr_sound_bindings(p,nullptr,nullptr,0)==1,"Clearing sound bindings failed");
        float position=1,length=1;require(mnr_sound_state(p,&position,&length)==0 && position==0 && length==0,"Stopped engine reports a playing clip");
        mnr_sound_play(p,2);mnr_sound_volume(p,1.5f);
        require(mnr_sound_seek(p,2,0.05f)==1 && mnr_sound_seek(p,0,0)==0
            && mnr_sound_seek(p,2,-1)==0 && mnr_sound_seek(p,2,std::numeric_limits<float>::quiet_NaN())==0,
            "Sound seek validation");
        mnr_sound_restart(p,2);
        mnr_sound_clear(p);
        require(mnr_sound_gain(p,2,0.5f)==0,"Clear kept clips");
        require(mnr_pick_paths(2,error,sizeof(error))==-1,"Invalid picker mode accepted");
    }
    require(!mnr_start(p,"",0,"TAG",3,2,40,5,-1,1,error,sizeof(error)),"Invalid input accepted");
    mnr_snapshot(p,&s,error,sizeof(error),1);require(s.state==5&&strstr(error,"Invalid audio settings"),"Error lost at ABI boundary");
    mnr_stop(p);mnr_snapshot(p,&s,error,sizeof(error),1);require(s.state==0&&s.muted==1,"Stop lost mute");
    const auto stale=mnr_begin_operation(p);const auto current=mnr_begin_operation(p);
    require(stale && current>stale,"Operation generation must advance");
    require(!mnr_start_generation(p,"",0,"TAG",3,2,40,5,-1,1,error,sizeof(error),stale),"Cancelled start accepted");
    mnr_snapshot(p,&s,error,sizeof(error),1);require(s.state==0 && s.muted==1,"Cancelled start changed state or Mute");
    require(mnr_start(p,"abc",40000,"TAG",3,2,40,5,-1,1,error,sizeof(error))==0,"Invalid length accepted");
    std::cout<<"BRIDGE CHECK PASSED: ABI, errors, controls, mute, epochs\n";
    return 0;
}catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}}
