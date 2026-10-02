#include "effects.hpp"
#include "audio.hpp"
#include <iostream>
#include <memory>
#include <array>
#include <chrono>
#include <limits>
static void require(bool ok,const char* message){if(!ok) throw std::runtime_error(message);}
int main() {try {
    {
        mic::NoiseGate gate;std::array<float,480> data{};
        data.fill(0.001f);
        require(gate.process(data.data(),data.size(),-40)==0.001f,"Gate lost its pre-gate meter");
        for(float v:data)require(v==0,"Closed gate leaked quiet microphone");
        data.fill(0.1f);gate.process(data.data(),data.size(),-40);
        require(data.front()>0 && data.front()<0.001f && data.back()==0.1f,"Gate attack is not a bounded ramp");
        data.fill(0.008f);gate.process(data.data(),data.size(),-40);
        for(float v:data)require(v==0.008f,"Gate hysteresis chopped quiet speech");
        for(unsigned frame=0;frame<12;++frame){data.fill(0.001f);gate.process(data.data(),data.size(),-40);require(data.back()==0.001f,"Gate hold cut a word ending");}
        data.fill(0.001f);gate.process(data.data(),data.size(),-40);
        require(data.front()>0 && data.front()<0.001f && data.back()==0,"Gate release is not a bounded ramp");
        data.fill(0.1f);gate.process(data.data(),data.size(),-40);
        require(data.back()==0.1f,"Gate did not reopen");
        for(unsigned i=0;i<data.size();++i)data[i]=0.2f*std::sin(i*0.1f);
        const auto dry=data;gate.process(data.data(),data.size(),-72);
        require(data==dry,"Disabled gate is not exact dry bypass");
        gate=mic::NoiseGate{};data.fill(0.5f);gate.process(data.data(),data.size(),0);
        for(float v:data)require(v==0,"Gate upper threshold ignored");
        // The common output mixer used by TAG and WASAPI receives a gated microphone,
        // while Discord and soundpad keep their own samples; final Mute is still downstream.
        data.fill(0.001f);gate.process(data.data(),data.size(),-40);
        auto microphone=data;data.fill(0.1f);std::array<float,480> sound{};sound.fill(0.2f);
        std::array<uint8_t,480> discord{};discord.fill(1);mic::OutputEffects mix;
        mix.process(data.data(),data.size(),1,1,false,false,discord.data(),0.08f,nullptr,microphone.data(),nullptr,sound.data());
        for(float v:data)require(std::isfinite(v) && std::abs(v-0.208f)<1e-6f,"Microphone gate muted Discord or soundpad");
        std::cout<<"noise_gate=passed bypass=exact hysteresis_hold_ramps=passed source_isolation=passed\n";
    }
    {
        // Quiet effect inputs stay intact, but every unmodified/live sample is gated.
        std::array<float,480> raw{},voice{},envelope{},data{};
        std::array<uint8_t,480> marks{};mic::NoiseGate gate;
        for(unsigned i=0;i<raw.size();++i)raw[i]=0.001f*std::sin(i*0.1f);
        voice=raw;gate.process(voice.data(),voice.size(),-40,envelope.data());
        for(float v:voice)require(v==0,"Quiet background voice escaped the gate");
        auto check=[&](bool discord=false){
            const auto before=data;const auto categories=marks;
            mic::NoiseGate::applyVoice(data.data(),data.size(),envelope.data(),marks.data(),discord);
            bool heard=false;
            for(unsigned i=0;i<data.size();++i){
                const bool effect=(categories[i]&~mic::ModifiedVoice)!=0;
                require(data[i]==((discord||effect)?before[i]:0),"Gate chopped playback or leaked live voice");
                require(!(marks[i]&mic::ModifiedVoice),"Internal voice marker leaked into the output queue");
                if(categories[i]&mic::ModifiedVoice)require(marks[i]&mic::ModifiedEffects,"RVC lost its monitor category");
                heard|=effect && std::abs(data[i])>1e-6f;
            }
            return heard;
        };
        data=raw;marks.fill(mic::ModifiedVoice);check(); // RVC remains live voice.
        data=raw;marks.fill(0);check(true);require(data==raw,"Discord was gated");
        marks.fill(0);mic::OutputEffects boost;data=raw;
        boost.process(data.data(),data.size(),1,3,true,false,nullptr,1,marks.data());
        require(check(),"Quiet boost effect was gated");
        mic::StutterEffect stutter;
        for(int frame=0;frame<5;++frame)stutter.feed(raw.data(),raw.size());
        data.fill(0);marks.fill(0);stutter.process(data.data(),data.size(),true,50,marks.data());
        require(check(),"Quiet stutter history was gated");
        for(unsigned held:{mic::HoldSlow,mic::HoldFast,mic::HoldReverse}){
            mic::PhraseEffect phrase;bool heard=false;
            data.fill(0);phrase.process(data.data(),data.size(),0,0.7f,1.5f,true,1,0);
            for(int frame=0;frame<180;++frame){
                data=raw;marks.fill(mic::ModifiedVoice);
                phrase.process(data.data(),data.size(),frame<40?held:0,0.7f,1.5f,true,1,0,true,marks.data());
                heard|=check();
                if(frame<40)for(float v:data)require(v==0,"Phrase recording leaked quiet live voice");
            }
            require(heard,"Quiet slow/fast/reverse playback was gated");
            require(!phrase.state(),"Phrase did not finish in gate regression");
        }
        for(float semitones:{0.0f,4.0f}){ // Shared pitch/formant and AutoTune playback stage.
            mic::PitchEffect pitch;bool heard=false;
            for(int frame=0;frame<40;++frame){
                data=raw;marks.fill(mic::ModifiedVoice);
                pitch.processAdvanced(data.data(),data.size(),semitones,semitones,true,marks.data());
                heard|=check();
            }
            require(heard,"Quiet pitch/AutoTune playback was gated");
        }
        mic::EchoEffect echo;std::array<float,480> tail{};bool heardEcho=false;
        for(int frame=0;frame<25;++frame){
            echo.process(frame==0?raw.data():nullptr,raw.size(),frame==0,60,3,50,100,tail.data());
            data=raw;marks.fill(0);check();
            mic::OutputEffects mixer;
            mixer.process(data.data(),data.size(),1,1,false,false,nullptr,1,nullptr,voice.data(),nullptr,nullptr,tail.data());
            for(unsigned i=0;i<data.size();++i){require(data[i]==tail[i],"Gate chopped an echo tail or leaked background voice");heardEcho|=std::abs(data[i])>1e-6f;}
        }
        require(heardEcho,"Quiet echo was not captured before the gate");
        mic::LastEffect replay;bool discord=false;
        data.fill(0);marks.fill(0);replay.process(data.data(),data.size(),marks.data(),discord,0,false,true,1,0,0);
        data=raw;marks.fill(mic::ModifiedEffects);check();
        replay.process(data.data(),data.size(),marks.data(),discord,mic::HoldStutter,false,true,1,0,0);
        data=raw;marks.fill(0);check();replay.process(data.data(),data.size(),marks.data(),discord,0,false,true,1,0,0);
        data=raw;marks.fill(0);check();
        require(replay.process(data.data(),data.size(),marks.data(),discord,0,false,true,1,0,1) && data==raw,"Gate chopped the last-effect replay");
        voice=raw;gate.process(voice.data(),voice.size(),-72,envelope.data());
        data=raw;marks.fill(0);mic::NoiseGate::applyVoice(data.data(),data.size(),envelope.data(),marks.data(),false);
        require(data==raw && voice==raw,"Disabled split gate is not exact dry bypass");
        std::cout<<"noise_gate_effect_playback=passed live_voice_rvc=passed quiet_hotkeys_replay_echo=passed\n";
    }
    {
        // Grain reverse: 50 % Hann overlap must add to unity (steady input stays steady),
        // stay bounded, reverse order inside a grain, and reset to silence.
        mic::GrainReverse reverse;std::vector<float> x(48000,0.5f);
        reverse.process(x.data(),480*20);reverse.process(x.data()+480*20,48000-480*20);
        for(unsigned i=mic::GrainReverse::Grain*2;i<48000;++i) require(std::abs(x[i]-0.5f)<1e-4f,"Grain reverse is not unity gain");
        reverse.reset();std::vector<float> ramp(mic::GrainReverse::Grain*3);
        for(unsigned i=0;i<ramp.size();++i) ramp[i]=float(i%mic::GrainReverse::Hop)/mic::GrainReverse::Hop;
        reverse.process(ramp.data(),unsigned(ramp.size()));
        for(float v:ramp) require(std::isfinite(v) && std::abs(v)<=1.0001f,"Grain reverse left the input range");
        // A rising sawtooth comes back falling: most steps inside the steady part go down.
        unsigned falling=0,rising=0;
        for(unsigned i=mic::GrainReverse::Grain*2+1;i<ramp.size();++i) (ramp[i]<ramp[i-1]?falling:rising)++;
        require(falling>rising*4,"Grain reverse did not reverse");
        reverse.reset();std::vector<float> quiet(960,0.f);reverse.process(quiet.data(),960);
        for(float v:quiet) require(v==0.f,"Grain reverse kept audio after reset");
    }
    {
        mic::EchoEffect echo;
        std::array<float,480> input{},wet{};
        std::vector<float> heard(480*20);
        for(int frame=0;frame<20;++frame){
            input.fill(0);if(frame==0)input[0]=0.5f;
            echo.process(input.data(),480,frame==0,60,3,50,100,wet.data());
            std::copy(wet.begin(),wet.end(),heard.begin()+frame*480);
        }
        for(size_t i=0;i<heard.size();++i){
            const float expected=i==2880?0.5f:i==5760?0.25f:i==8640?0.125f:0;
            require(std::abs(heard[i]-expected)<1e-6f,"Echo duplicated dry input, fed back, or cut its tail");
        }
        std::array<float,1> dry{0.25f},micTail{0},discordTail{0.5f},mainOnly{},tailOnly{};
        std::array<uint8_t,1> source{0},category{0};
        mic::OutputEffects mixer;
        mixer.process(dry.data(),1,1,1,false,false,source.data(),0.08f,category.data(),nullptr,
            mainOnly.data(),nullptr,micTail.data(),discordTail.data(),tailOnly.data());
        require(std::abs(dry[0]-0.29f)<1e-6f && std::abs(tailOnly[0]-0.04f)<1e-6f &&
            mic::previewQueued(mainOnly[0],category[0],1,0,false,1,1,true).value==0,
            "Discord echo tail gain or dry monitor separation failed");
        echo.reset();input.fill(0);echo.process(input.data(),480,true,60,3,50,100,wet.data());
        require(std::all_of(wet.begin(),wet.end(),[](float v){return v==0;}),"Old echo survived reset");
        // Maximum interval/repeats must survive ring wrap and finish after release.
        echo.reset();
        for(int frame=0;frame<3250;++frame){
            input.fill(0);if(frame==0||frame==1600)input[0]=0.5f;
            echo.process(input.data(),480,frame<=1600,2000,8,90,100,wet.data());
            for(int i=0;i<480;++i){
                float expected=0;
                for(int start:{0,1600})for(int tap=1;tap<=8;++tap)
                    if(frame==start+tap*200&&i==0)expected+=0.5f*std::pow(0.9f,tap-1);
                require(std::abs(wet[i]-expected)<1e-6f,"Long echo interval/tail or ring wrap failed");
            }
        }
        require(!echo.active(),"Maximum echo tail did not finish");
        std::cout<<"echo=passed taps=3/8 interval=60/2000ms tail=16s dry=excluded\n";
    }
    {
        mic::StutterEffect stutter;
        std::array<float,480> frame{};
        for(int block=0;block<30;++block){for(int i=0;i<480;++i)frame[i]=(block*480+i)/14400.0f;stutter.feed(frame.data(),480);}
        frame.fill(0);stutter.process(frame.data(),480,true,50);
        frame.fill(0);stutter.process(frame.data(),480,true,50);
        require(std::abs(frame[300]-12780/14400.0f)<1e-5f,"Stutter did not capture the last 50 ms");
        for(int i=0;i<4;++i){frame.fill(0);stutter.process(frame.data(),480,true,50);}
        frame.fill(0);stutter.process(frame.data(),480,true,50);
        require(std::abs(frame[300]-12780/14400.0f)<1e-5f,"Stutter loop changed its captured phrase");
        frame.fill(0.2f);stutter.process(frame.data(),480,false,50);
        frame.fill(0.2f);stutter.process(frame.data(),480,false,50);
        require(frame[479]==0.2f,"Stutter did not release to live voice");
        std::cout<<"stutter=passed\n";
    }
    {
        mic::AutoTunePitch tune;
        std::array<float,480> voice{};float correction=0;
        for(int block=0;block<100;++block){
            for(int i=0;i<480;++i)voice[i]=0.3f*std::sin(6.28318530718f*452*(block*480+i)/48000);
            correction=tune.process(voice.data(),480,true,0,0,1,80,100);
        }
        require(correction< -0.25f&&correction> -0.8f,"AutoTune missed A in C major");
        voice.fill(0);
        for(int i=0;i<80;++i){
            const float previous=correction;
            correction=tune.process(voice.data(),480,true,0,0,1,80,100);
            if(!tune.voiced())require(std::abs(correction-previous)<0.1f,"AutoTune jumped on lost voicing");
        }
        require(std::abs(correction)<0.00001f&&!tune.voiced(),"AutoTune did not relax to neutral");
        // Hold through vowels, silence and consonant-like noise without restarting the shifter.
        mic::PitchEffect continuous;unsigned random=12345;bool heardTail=false;
        for(int block=0;block<260;++block){
            for(int i=0;i<480;++i){
                random=random*1664525u+1013904223u;
                voice[i]=block<100||block>=160?0.3f*std::sin(6.28318530718f*452*(block*480+i)/48000):
                    block<120?0:0.15f*(static_cast<float>(random>>8)/8388608.0f-1);
            }
            correction=tune.process(voice.data(),480,true,0,0,1,80,100);
            continuous.processAdvanced(voice.data(),480,correction,0,true);
            for(float sample:voice){
                require(std::isfinite(sample)&&std::abs(sample)<=1,"AutoTune transition escaped bounds");
                if(block>=104&&block<120&&std::abs(sample)>0.01f)heardTail=true;
            }
            if(block>20)require(continuous.active(),"AutoTune restarted during a held phrase");
        }
        require(heardTail,"AutoTune cut the delayed vowel tail at loss of voicing");
        voice.fill(0.2f);continuous.processAdvanced(voice.data(),480,0,0,false);
        voice.fill(0.2f);continuous.processAdvanced(voice.data(),480,0,0,false);
        require(voice.back()==0.2f&&!continuous.active(),"AutoTune release lost dry bypass");
        continuous.reset();voice.fill(0);continuous.processAdvanced(voice.data(),480,0,0,true);
        require(std::all_of(voice.begin(),voice.end(),[](float v){return v==0;}),"AutoTune reset leaked old source");
        mic::PitchEffect formant;unsigned crossings=0;float previous=0;
        for(int block=0;block<180;++block){
            for(int i=0;i<480;++i)voice[i]=0.2f*std::sin(6.28318530718f*440*(block*480+i)/48000);
            formant.processAdvanced(voice.data(),480,0,6,true);
            if(block>80)for(float sample:voice){if(previous<=0&&sample>0)++crossings;previous=sample;}
        }
        require(std::abs(crossings/0.99f-440)<20,"Formant shift changed fundamental pitch");
        std::cout<<"autotune=passed formant=passed\n";
    }
    std::array<float,480> original{},data{};
    {
        mic::LastEffect replay;std::array<uint8_t,480> modified{};bool discord=false;
        require(mic::sourceRecordFlags(mic::HoldEcho|mic::HoldAutoTune,true)==
            ((mic::HoldEcho|mic::HoldAutoTune)<<4),"Discord live activity bits lost their source");
        auto run=[&](float value,unsigned held,unsigned request,bool valid=true,unsigned epoch=1,unsigned cancel=0){
            data.fill(value);modified.fill(held?((held&33)?2:1):0);
            return replay.process(data.data(),480,modified.data(),discord,held,false,valid,epoch,cancel,request);
        };
        run(0,0,0);require(!run(0,0,1),"Empty replay must do nothing");
        // Boost is a gain change, not a clip: holding it records nothing to replay or save.
        run(0.3f,1,1);run(0,0,1);
        require(!run(0.1f,0,2) && !replay.finished(),"Boost must not be recorded");
        run(0.3f,2,2);run(0,0,2);
        require(replay.finished() && replay.count()==480,"Finished recording was not published");
        require(!replay.finished(),"Reading a finished recording must clear it");
        std::vector<float> saved;replay.copyRecording(saved,0.08f);
        require(saved.size()==480 && saved[100]==0.3f,"Microphone recording was attenuated");
        require(run(0.1f,0,3) && data[100]==0.3f && !discord,"Microphone effect replay failed");
        require(modified[100]==1,"Replay lost its monitor category");
        require(run(0,0,4) && data[100]==0.3f,"Replay overwrote the saved clip");
        discord=true;run(0.6f,64,4);run(0,0,4);discord=false;
        replay.copyRecording(saved,0.08f);
        require(saved.size()==480 && std::abs(saved[100]-0.6f*0.08f)<1e-6,"Discord recording lost its output gain");
        require(run(0.1f,0,5) && data[100]==0.6f && discord,"Latest Discord effect did not replace microphone clip");
        require(modified[100]==1,"Discord replay leaked into another category");
        float replayOutput=data[100],replayOnly=0;uint8_t replaySource=1,replayCategory=modified[100];
        mic::OutputEffects replayMix;
        replayMix.process(&replayOutput,1,1,1,false,false,&replaySource,0.08f,&replayCategory,nullptr,&replayOnly);
        const auto replayPreview=mic::previewQueued(replayOnly,replayCategory,1,0,false,1,1,true);
        require(std::abs(mic::previewSample(replayPreview,1,1,true)-0.048f)<1e-6,"Discord hotkey replay missing from effects monitor");
        discord=false;run(0.2f,mic::sourceRecordFlags(mic::HoldEcho,false),5);
        discord=true;run(0.6f,mic::sourceRecordFlags(mic::HoldEcho,true),5);run(0,0,5);
        replay.copyRecording(saved,0.08f);
        require(saved.size()==480 && std::abs(saved[100]-0.048f)<1e-6,"Same live effect on another source did not replace recording");
        discord=false;
        run(0.2f,2,5);run(0,0,5,false);
        require(!run(0,0,6),"Interrupted recording survived reset");
        run(0.2f,2,6);run(0.2f,2,6);run(0,0,6);
        require(run(0,0,7),"Replay did not start");
        require(!run(0,0,7,true,1,1),"Cancel did not stop replay");
        // Long holds stay bounded; playback ends after the 20-second slot.
        for(int i=0;i<2100;++i)run(0.2f,2,7,true,1,1);
        run(0,0,7,true,1,1);
        require(run(0,0,8,true,1,1),"Bounded replay failed to start");
        int blocks=1;while(run(0,0,8,true,1,1)){require(++blocks<=2000,"Unbounded replay");}
        require(blocks==2000,"Replay capacity changed");
        mic::LastEffect echoReplay;std::array<float,480> echoCapture{};std::array<uint8_t,480> echoFlags{};
        bool echoSource=false;data.fill(0);modified.fill(0);
        echoReplay.process(data.data(),480,modified.data(),echoSource,0,false,true,1,0,0);
        for(float level:{0.1f,0.2f}){
            data.fill(level==0.1f?0.5f:0.7f);echoCapture.fill(level);echoFlags.fill(mic::ModifiedEffects);
            echoReplay.process(data.data(),480,modified.data(),echoSource,level==0.1f?mic::HoldEcho:0,
                false,true,1,0,0,echoCapture.data(),echoFlags.data(),true);
        }
        data.fill(0.7f);echoCapture.fill(0);echoFlags.fill(0);
        echoReplay.process(data.data(),480,modified.data(),echoSource,0,false,true,1,0,0,
            echoCapture.data(),echoFlags.data(),false);
        echoReplay.copyRecording(saved,1);
        require(echoReplay.finished()&&saved.size()==960&&saved[100]==0.1f&&saved[580]==0.2f,
            "Echo replay captured returned dry voice or lost its tail");
        std::array<float,480> mic{},only{};std::array<uint8_t,480> sources{};sources.fill(1);mic.fill(0.25f);
        {
            mic::OutputEffects output;
            for(int i=0;i<2;++i){data.fill(0.5f);output.process(data.data(),480,1,1,false,false,sources.data(),0,modified.data(),mic.data(),only.data());}
            require(data[479]==0.25f && only[479]==0,"Discord volume muted microphone or leaked it into effects monitor");
            data.fill(0.5f);output.process(data.data(),480,1,1,false,false,sources.data(),0.08f,modified.data(),mic.data(),only.data());
            require(std::abs(data[479]-0.29f)<1e-6 && std::abs(only[479]-0.04f)<1e-6,"Microphone/Discord mix wrong");
        }
        std::cout<<"last_effect_replay=passed microphone_mix=passed\n";
    }
    {
        // Soundpad: one clip at a time, quick double press restarts, same press stops, 5 ms fades,
        // and the mix bypasses Discord gain while staying out of the effect-only preview.
        mic::SoundPlayer player;std::array<float,480> sound{};
        auto clip=std::make_shared<mic::SoundClip>();clip->samples.assign(4800,0.5f);clip->gain=1;
        mic::SoundPlayer fromCursor;fromCursor.start(7,clip);
        require(fromCursor.seek(7,2400) && std::abs(fromCursor.position()-0.05f)<1e-6f,"Initial seek must start at the cursor");
        fromCursor.render(sound.data(),480,1);require(sound[0]==0.5f,"Initial seek leaked the clip beginning");
        auto loopClip=std::make_shared<mic::SoundClip>();loopClip->samples.assign(4800,0.5f);
        loopClip->loopRange=(uint64_t(1200)<<32)|3600;loopClip->independentVolume=true;
        mic::SoundPlayer looping;looping.start(8,loopClip);
        require(looping.seek(8,1200) && std::abs(looping.position()-0.025f)<1e-6f,"Loop seek missed its start");
        for(int i=0;i<6;++i)looping.render(sound.data(),480,0.05f);
        require(looping.playing()==8 && looping.position()>0.025f && looping.position()<0.075f
            && sound[300]>0.49f,"Loop must wrap in DSP without soundpad-volume attenuation");
        looping.render(sound.data(),480,1,true);
        require(sound[300]==0,"Mute must silence a looping clip");
        auto request=[&](unsigned id,uint64_t serial,bool restart){const auto packed=mic::SoundPlayer::pack(id,serial,restart);const auto lookup=player.request(packed);if(lookup){player.commit(packed);player.start(lookup,clip);}return lookup;};
        player.render(sound.data(),480,1);require(sound[0]==0 && player.playing()==0,"Idle player produced audio");
        require(request(3,1,false)==3,"First press must look up the clip");
        require(request(3,1,false)==0,"Repeated serial must be ignored");
        std::array<uint8_t,480> soundFlags{};
        player.render(sound.data(),480,1.0f,false,soundFlags.data());require(sound[100]==0.5f && soundFlags[100]==0 && player.playing()==3 && player.length()==0.1f,"Clip did not play");
        player.render(sound.data(),480,0.5f);require(std::abs(sound[100]-0.25f)<1e-6,"Volume ignored");
        require(!player.seek(4,2400) && player.seek(3,2400),"Seek must target the playing clip");
        player.render(sound.data(),480,1);
        require(sound[0]==0.5f && sound[239]<0.02f && sound[240]<0.001f
            && sound[479]>0.49f && player.position()>0.05f && player.position()<0.06f,
            "Seek must fade out, jump, then fade in");
        player.render(sound.data(),480,1); // finish the 5 ms fade-in before the restart check
        require(request(3,2,true)==3,"Quick double press must restart");
        player.render(sound.data(),480,1);
        require(sound[0]==0.5f && sound[239]<0.02f && sound[240]<0.001f && sound[241]==0.5f && player.position()<0.006f,"Restart must fade out then start from the beginning");
        require(request(3,3,false)==0,"Second press of the playing clip must stop it");
        player.render(sound.data(),480,1);require(sound[0]==0.5f && sound[120]<0.26f && sound[300]==0 && player.playing()==0,"Stop did not fade to silence");
        mic::SoundPlayer queued;queued.start(3,clip);queued.render(sound.data(),480,1);
        queued.start(4,clip);queued.stop();queued.render(sound.data(),480,1);
        require(queued.playing()==0,"Stop played a queued replacement");
        request(3,4,false);for(int i=0;i<12;++i)player.render(sound.data(),480,1);require(player.playing()==0,"Clip did not end");
        require(request(0,5,false)==0 && request(0,5,false)==0,"Stop request must not look anything up");
        mic::OutputEffects output;std::array<uint8_t,480> sources{};sources.fill(1);std::array<float,480> only{},mixed{};mixed.fill(0.25f);
        for(int i=0;i<2;++i){data.fill(0.5f);output.process(data.data(),480,1,1,false,false,sources.data(),0.08f,nullptr,nullptr,only.data(),mixed.data());}
        require(std::abs(data[479]-0.29f)<1e-6 && std::abs(only[479]-0.04f)<1e-6,"Soundpad must bypass Discord gain and stay out of the effect preview");
        mic::SoundPlayer recordingPlayer;auto recording=std::make_shared<mic::SoundClip>();
        recording->samples.assign(960,0.08f);
        const auto recordingRequest=mic::SoundPlayer::pack(mic::recordingClipIdBase,1,false);
        require(recordingPlayer.request(recordingRequest)==mic::recordingClipIdBase,"Recording request lost");
        recordingPlayer.commit(recordingRequest);recordingPlayer.start(mic::recordingClipIdBase,recording);
        std::array<uint8_t,480> recordingFlags{};
        recordingPlayer.render(sound.data(),480,0.04f,false,recordingFlags.data());
        require(sound[100]==0.08f,"Recording was reduced by soundpad volume");
        require(recordingFlags[100]==1,"Recording identity was lost during playback");
        const auto recordingPreview=mic::previewQueued(0,0,7,sound[100],recordingFlags[100],1,7,true);
        require(std::abs(mic::previewSample(recordingPreview,1,7,true)-0.08f)<1e-6,"Recent recording missing from effects monitor");
        require(std::abs(mic::previewSample(mic::previewQueued(0,0,7,sound[100],true,4,7,true),4,7,true)-0.08f)<1e-6,"Recent recording missing from sound monitor");
        require(std::abs(mic::previewSample(mic::previewQueued(0,0,7,sound[100],true,5,7,true),5,7,true)-0.08f)<1e-6,"Recent recording doubled with both monitor boxes");
        recordingPlayer.render(sound.data(),480,0.04f,true,recordingFlags.data());
        require(sound[100]==0,"Muted recording was audible");
        require(mic::previewSample(mic::previewQueued(0,0,7,sound[100],recordingFlags[100],1,7,false),1,7,true)==0,"Muted recording leaked into effects monitor");
        mic::RoutedSample routed{0.1f,0,0,7,0,0.3f};
        require(mic::previewSample(routed,mic::ModifiedSound,7,true)==0,"ModifiedSound is a monitor mask bit, never a sample category");
        // Producer + consumer of the effects-only monitor: "hear sounds" must survive the second filter.
        for(uint8_t mask:{4,5,6,7}){
            const auto queued=mic::previewQueued(0.1f,0,7,0.3f,false,mask,7,true);
            require(mic::previewSample(queued,mask,7,true)==0.3f,"Clip sample dropped by the monitor consumer");
            require(mic::previewSample(queued,static_cast<uint8_t>(mask&3),7,true)==0,"Clip audible without the sound mask");
        }
        const auto effect=mic::previewQueued(0.1f,1,7,0.3f,false,5,7,true);
        require(std::abs(mic::previewSample(effect,5,7,true)-0.4f)<1e-6,"Effect + clip preview mix");
        require(mic::previewSample(mic::previewQueued(0.1f,0,7,0.3f,false,4,7,false),4,7,true)==0,"Muted clip leaked into preview");
        require(mic::previewSample(mic::previewQueued(0.1f,0,7,0.3f,false,1,7,true),1,7,true)==0,"Clip leaked into effects-only preview");
        std::cout<<"soundpad=passed double_press_restart=passed fade_samples=240\n";
    }
    for(unsigned i=0;i<480;++i) original[i]=std::sin(i*0.17f)*0.9f;
    {
        // Same final preview gate is used by TAG, WASAPI and the monitor consumer.
        for(uint8_t source:{0,1})for(uint8_t flags:{0,1,2,3})for(uint8_t mask:{0,1,2,3}){
            mic::RoutedSample sample{0.25f,source,flags,7};
            require(mic::previewSample(sample,mask,7,true)==((flags&mask)?0.25f:0),"Independent monitor filter failed");
            require(mic::previewSample(sample,mask,8,true)==0,"Old epoch leaked into preview");
            require(mic::previewSample(sample,mask,7,false)==0,"Muted/inactive output leaked into preview");
        }
        mic::OutputEffects boost;std::array<uint8_t,480> marked{};
        data.fill(0.01f);boost.process(data.data(),480,1,3,true,true,nullptr,1,marked.data());
        require(marked[479]==2,"Boost must not enable the other-effects preview");
        marked.fill(1);data.fill(0.01f);boost.process(data.data(),480,1,3,true,true,nullptr,1,marked.data());
        require(marked[479]==3,"Boost erased another active effect category");
        for(int i=0;i<2;++i){marked.fill(0);data.fill(0.01f);boost.process(data.data(),480,1,3,false,true,nullptr,1,marked.data());}
        require(marked[479]==0,"Released boost kept enabling preview");
        std::cout<<"independent_effect_preview=passed categories=4 mute_epoch=passed\n";
    }
    mic::OutputEffects gain;mic::PitchEffect pitch;
    data=original;gain.process(data.data(),480,1,3,false);pitch.process(data.data(),480,-5,false);
    require(data==original,"Dry bypass changed samples");
    data=original;gain.process(data.data(),480,1,1,true);pitch.process(data.data(),480,0,true);
    require(data==original,"Neutral controls changed samples");
    for(int j=0;j<4;++j){data=original;gain.process(data.data(),480,1,20,true);}
    for(float v:data) require(std::isfinite(v)&&std::abs(v)<=0.891001f,"Saturation ceiling");
    data.fill(std::numeric_limits<float>::quiet_NaN());gain.process(data.data(),480,1,20,true);
    for(float v:data) require(std::isfinite(v),"Non-finite gain output");
    {
        mic::OutputEffects hard;
        data=original;hard.process(data.data(),480,1,20,false,true);require(data==original,"Overload leaked without hold");
        data=original;hard.process(data.data(),480,1,1,true,true);require(data==original,"Neutral overload changed audio");
        for(int f=0;f<3;++f){data.fill(0.1f);hard.process(data.data(),480,1,3,true,true);}
        for(float v:data)require(std::abs(v-0.891f)<1e-6,"Overload did not hard clip");
        data.fill(-0.1f);hard.process(data.data(),480,1,3,true,true);
        for(float v:data)require(std::abs(v+0.891f)<1e-6,"Overload is not symmetric");
        data.fill(0.01f);hard.process(data.data(),480,1,3,true,true);
        for(float v:data)require(std::abs(v-0.36f)<1e-6,"Overload drive must be 12x before clipping");
        data.fill(-0.1f);hard.process(data.data(),480,1,3,true,true);
        float previous=-0.891f;
        for(int i=0;i<480;++i){float v=-0.1f;hard.process(&v,1,1,3,true,false);require(std::abs(v-previous)<0.003f,"Overload toggle discontinuity");previous=v;}
        require(std::abs(previous+0.891f*std::tanh(0.3f/0.891f))<1e-6,"Overload did not return to soft saturation");
        data.fill(std::numeric_limits<float>::infinity());hard.process(data.data(),480,1,20,true,true);
        for(float v:data)require(std::isfinite(v)&&std::abs(v)<=0.891001f,"Overload output bounds");
        for(int f=0;f<2;++f){data=original;hard.process(data.data(),480,1,20,false,true);}
        require(data==original,"Overload release did not return to dry");
        std::cout<<"overload=passed hard_clip=0.891 transition_ms=10\n";
    }
    // Steady-state zero crossings verify pitch without trusting the implementation formula alone.
    {
        std::array<uint8_t,480> sources{};
        for(unsigned i=0;i<480;++i)sources[i]=static_cast<uint8_t>(i%2);
        for(bool overload:{false,true}){
            mic::OutputEffects reference,limited;
            auto full=original;data=original;
            reference.process(full.data(),480,1,20,true,overload);
            limited.process(data.data(),480,1,20,true,overload,sources.data(),0.16f);
            for(int f=0;f<4;++f){
                full=original;data=original;
                reference.process(full.data(),480,1,20,true,overload);
                limited.process(data.data(),480,1,20,true,overload,sources.data(),0.16f);
                for(unsigned i=0;i<480;++i)require(std::abs(data[i]-full[i]*(sources[i]?0.16f:1.0f))<1e-6,"Discord volume must apply after distortion without changing microphone");
            }
            data=original;limited.process(data.data(),480,1,20,true,overload,sources.data(),0);
            data=original;limited.process(data.data(),480,1,20,true,overload,sources.data(),0);
            for(unsigned i=1;i<480;i+=2)require(data[i]==0,"Discord 0% did not silence output");
        }
        mic::Ring<4,mic::RoutedSample> queue;
        mic::RoutedSample first[]={{1,0},{2,1},{3,1},{4,0}},last[]={{5,0},{6,1}},read[4]{};
        require(queue.push(first,4)&&queue.pop(read,2)&&queue.push(last,2),"Routed queue wrap failed");
        require(queue.pop(read,4)&&read[0].value==3&&read[0].discord==1&&read[1].value==4&&read[1].discord==0&&read[3].value==6&&read[3].discord==1,"Source metadata separated from queued audio");
        std::cout<<"discord_volume=passed default=100% gain=8% max=200% post_effects=true\n";
    }
    for(int semitones:{-12,-5,7,12}) {
        mic::PitchEffect p;unsigned crossings=0;float previous=0;double maxMs=0;
        for(unsigned frame=0;frame<300;++frame) {
            for(unsigned i=0;i<480;++i) data[i]=0.2f*std::sin(2*3.141592653589793*440*(frame*480+i)/48000);
            auto begin=std::chrono::steady_clock::now();p.process(data.data(),480,semitones,true);
            maxMs=std::max(maxMs,std::chrono::duration<double,std::milli>(std::chrono::steady_clock::now()-begin).count());
            for(float v:data) {require(std::isfinite(v)&&std::abs(v)<=1,"Pitch bounds");if(frame>=100 && previous<=0&&v>0)++crossings;previous=v;}
        }
        const double measured=crossings/2.0,expected=440*std::exp2(semitones/12.0);
        require(std::abs(measured-expected)<expected*0.015,"Pitch frequency mismatch");
        std::cout<<"pitch="<<semitones<<" measured_hz="<<measured<<" delay_ms="<<p.delayMs()<<" max_block_ms="<<maxMs<<'\n';
        data=original;p.process(data.data(),480,semitones,false);
        data=original;p.process(data.data(),480,semitones,false);require(data==original,"Release did not return to dry");
        data.fill(0);p.process(data.data(),480,semitones,true);
        for(float v:data) require(v==0,"Previous pitch tail replayed");
    }
    // Repeated short presses must never activate an unready shifter or leak old frames.
    for(int i=0;i<100;++i) {data=original;pitch.process(data.data(),480,-5,true);data=original;pitch.process(data.data(),480,-5,false);require(data==original,"Short pitch tap polluted dry audio");}
    const auto sample=mic::packHeld(1000,7,1023);
    require(mic::heldFlags(sample,7,1100,true)==1023,"Fresh hold lost");
    require(!mic::heldFresh(mic::packHeld(1000,7,0,false),7,1100),"Ineligible sample could release a phrase");
    require(mic::heldFlags(sample,7,1251,true)==0,"Stale heartbeat remained active");
    require(mic::heldFlags(sample,8,1100,true)==0,"Old session press remained active");
    require(mic::heldFlags(sample,7,1100,false)==0,"Ineligible press accepted");
    {
        mic::HoldLatch all;unsigned keys[18]{};bool pressed[18]{};
        for(unsigned i=0;i<18;++i)keys[i]=80+i;
        all.update(1,true,keys,pressed,0,false);
        std::fill(std::begin(pressed),std::end(pressed),true);
        const auto flags=all.update(1,true,keys,pressed,0,false);
        require(flags==mic::HoldAllMask && !(flags&((1<<12)|(1<<16))),"Active hotkey mask retained Granular");
        require(mic::heldFlags(mic::packHeld(1000,1,flags),1,1100,true)==flags,"New hold bits were lost in timestamp packing");
        require(all.update(1,true,keys,pressed,0,false,false)==mic::HoldMicMask,"Unavailable Discord retained a new hold");
    }
    {
        mic::HoldLatch noise;unsigned key[]={119|256};bool down[]={true};
        auto strength=[&](uint64_t now,unsigned epoch,bool eligible,unsigned mods){
            const auto flags=noise.update(epoch,eligible,key,down,mods,false);
            return mic::heldIntensity(1.05f,0.15f,mic::packHeld(now,epoch,flags,eligible),epoch,now,eligible);
        };
        require(strength(1000,1,true,1)==1.05f,"Noise hold armed without fresh press");
        down[0]=false;require(strength(1008,1,true,1)==1.05f,"Released key changed noise level");
        down[0]=true;require(strength(1016,1,true,1)==0.15f,"Noise hold did not select alternate level");
        require(strength(1024,1,true,0)==1.05f,"Noise hold ignored modifiers");
        require(strength(1032,1,false,1)==1.05f,"Mute/inactive output kept alternate noise level");
        require(strength(1040,1,true,1)==1.05f,"Noise hold rearmed across mute without release");
        down[0]=false;strength(1048,1,true,1);down[0]=true;
        require(strength(1056,1,true,1)==0.15f,"Noise hold did not rearm after release");
        require(strength(1064,2,true,1)==1.05f,"New epoch kept alternate noise level");
        const auto held=mic::packHeld(1000,1,1);
        require(mic::heldIntensity(1.05f,0.15f,held,1,1251,true)==1.05f,"Stale noise hold did not restore normal level");
        require(mic::heldIntensity(1.05f,0.15f,held,2,1100,true)==1.05f,"Old noise epoch survived reset");
        for(float alternate:{0.0f,2.0f})require(mic::heldIntensity(1.05f,alternate,held,1,1100,true)==alternate,"Noise alternate range changed");
        std::cout<<"noise_hold=passed release_modifiers_mute_epoch_stale=passed\n";
    }
    mic::HoldLatch latch;unsigned keys[]={119,120|(1<<8)};bool pressed[]={true,false};
    require(latch.update(1,true,keys,pressed,0,false)==0,"Held key armed on start");
    pressed[0]=false;latch.update(1,true,keys,pressed,0,false);pressed[0]=true;
    require(latch.update(1,true,keys,pressed,0,false)==1,"Fresh key not detected");
    require(latch.update(1,true,keys,pressed,1,false)==0,"Modifier subset fired");
    pressed[1]=true;require(latch.update(1,true,keys,pressed,1,false)==2,"Chord failed");
    require(latch.update(2,true,keys,pressed,1,false)==0,"Epoch failed to disarm");
    pressed[0]=pressed[1]=false;keys[1]=120;latch.update(2,true,keys,pressed,0,false);
    pressed[0]=pressed[1]=true;require(latch.update(2,true,keys,pressed,0,false)==3,"Simultaneous effects failed");
    latch.update(2,false,keys,pressed,0,false);require(latch.update(2,true,keys,pressed,0,false)==0,"Mute/lock rearmed held keys");
    // Engine rings are ~800 KB: keep them off the 1 MB default stack.
    auto engine=std::make_unique<mic::Engine>();auto& e=*engine;
    e.heldSample=mic::packHeld(GetTickCount64(),0,15);require(e.held()==0,"Stopped engine accepted hold");
    auto epoch=e.effectEpoch.load();e.releaseEffects();require(e.heldSample==0&&e.effectEpoch==epoch+1,"Hold reset");
    for(float speed:{0.5f,0.7f,1.5f,2.0f}) {
        mic::PhraseEffect phrase;const unsigned key=speed<1?4:8;
        data=original;phrase.process(data.data(),480,0,speed,speed,true,1,0);require(data==original,"Phrase dry bypass changed audio");
        for(unsigned frame=0;frame<120;++frame){
            for(unsigned i=0;i<480;++i)data[i]=0.2f*std::sin(2*3.141592653589793*440*(frame*480+i)/48000);
            const auto dry=data;
            phrase.process(data.data(),480,frame<100?key:0,speed,speed,true,1,0);
            require(data==dry,"Speed recording muted the microphone");
        }
        require(phrase.state()==(key==4?3:4),"Release did not start phrase playback");
        unsigned samples=0,crossings=0,first=0,last=0;float previous=0;
        while(phrase.state()){
            float v=0;phrase.process(&v,1,0,speed,speed,true,1,0);
            require(std::isfinite(v)&&std::abs(v)<=1,"Phrase bounds");
            if(previous<=0&&v>0&&samples>480){if(!crossings)first=samples;last=samples;++crossings;}
            previous=v;++samples;require(samples<120000,"Unbounded phrase playback");
        }
        const auto expected=std::ceil(57600.0/speed);
        const double measured=(crossings-1)*48000.0/(last-first);
        require(std::abs(samples-expected)<=1,"Phrase duration mismatch");
        require(std::abs(measured-440*speed)<2,"Coupled speed/pitch frequency mismatch");
        std::cout<<"phrase_speed="<<speed<<" samples="<<samples<<" hz="<<measured<<'\n';
        data.fill(0);phrase.process(data.data(),480,key,speed,speed,true,1,0);
        phrase.process(data.data(),480,0,speed,speed,false,1,0);require(phrase.state()==0,"Stale heartbeat played a phrase");
        for(int frame=0;frame<30;++frame){data.fill(0);phrase.process(data.data(),480,0,speed,speed,true,1,0);for(float v:data)require(v==0,"Old phrase tail replayed");}
        data.fill(0.2f);phrase.process(data.data(),480,key,speed,speed,true,1,0);
        phrase.process(data.data(),480,key,speed,speed,true,1,1);require(phrase.state()==0,"Cancel failed");
        phrase.process(data.data(),480,key,speed,speed,true,1,1);require(phrase.state()==0,"Cancel rearmed held key");
    }
    {
        mic::PhraseEffect phrase;data.fill(0);phrase.process(data.data(),480,0,0.7f,1.5f,true,1,0);
        for(unsigned i=0;i<1100;++i){data.fill(0.1f);phrase.process(data.data(),480,4,0.7f,1.5f,true,1,0);}
        require(phrase.state()==7 && phrase.seconds()==10,"Phrase recording limit");
        phrase.process(data.data(),480,12,0.7f,1.5f,true,1,0);require(!phrase.state(),"Conflicting speed keys must cancel");
        phrase.process(data.data(),480,8,0.7f,1.5f,true,1,0);require(!phrase.state(),"Conflict must require both releases");
    }
    unsigned allKeys[]={119,120,121,122,123,124,125,126,127,128};bool allPressed[10]{};mic::HoldLatch five;
    five.update(1,true,allKeys,allPressed,0,false);for(auto& p:allPressed)p=true;
    require(five.update(1,true,allKeys,allPressed,0,false)==1023,"Ten independent holds failed");
    require(five.update(1,true,allKeys,allPressed,0,false,false)==31,"Unavailable Discord keys remained active");
    require(five.update(1,true,allKeys,allPressed,0,false,true)==31,"Discord rearmed before key release");
    {
        mic::SourceRouting route;
        require(!route.select(0,false),"Default source must be microphone");
        for(unsigned key:{32u,64u})require(route.select(key,false),"Discord live effect lost its source");
        for(unsigned key:{4u,8u,16u}){
            route={};require(route.select(key<<5,false),"Discord phrase did not select source");
            require(route.phraseFlags==key,"Discord phrase effect mapping");
            require(route.select(0,true),"Discord phrase source lost after release");
            require(!route.select(0,false),"Discord source leaked after phrase");
            require(!route.select(key,false),"Mic phrase source changed");
            require(!route.select(32,true),"Discord boost changed an existing mic phrase source");
            route.select(key|(key<<5),true);require(route.phraseFlags==28,"Conflicting source keys did not cancel");
        }
        pitch.reset();data.fill(0);pitch.process(data.data(),480,-5,true);
        for(float v:data)require(v==0,"Pitch leaked previous source after reset");
    }
    for(bool reverseLive:{true,false}) {
        mic::PhraseEffect phrase;data.fill(0);phrase.process(data.data(),480,0,0.7f,1.5f,true,1,0);
        std::vector<float> recorded;
        for(unsigned frame=0;frame<10;++frame){
            for(unsigned i=0;i<480;++i)data[i]=0.7f*std::sin((frame*480+i)*0.031f)+0.1f*std::cos((frame*480+i)*0.013f);
            recorded.insert(recorded.end(),data.begin(),data.end());
            const auto dry=data;
            phrase.process(data.data(),480,16,0.7f,1.5f,true,1,0,reverseLive);
            if(reverseLive)require(data==dry,"Microphone reverse must pass live speech unchanged");
            else for(float v:data)require(v==0,"Discord reverse leaked original speech during recording");
        }
        for(unsigned i=0;i<7200;++i){
            float v=0.9f;phrase.process(&v,1,0,0.7f,1.5f,true,1,0);
            require(v==0,"Reverse pause must be silent");
            require(phrase.state()==(i==7199?12:10),"Reverse delay must be exactly 150 ms");
        }
        for(size_t i=0;i<recorded.size();++i){
            float v=0.9f;phrase.process(&v,1,0,0.7f,1.5f,true,1,0);
            const double end=std::clamp((recorded.size()-i-1)/std::min(2880.0,recorded.size()/2.0),0.0,1.0);
            const double fade=std::min(i/480.0,end*end*(3-2*end));
            const auto expected=recorded[recorded.size()-1-i]*fade;
            require(std::isfinite(v)&&std::abs(v-expected)<1e-6,"Reverse order, level or duration mismatch");
        }
        require(!phrase.state(),"Reverse playback did not finish");
        for(int frame=0;frame<26;++frame){data.fill(0);phrase.process(data.data(),480,frame<10?16:0,0.7f,1.5f,true,1,0);for(float v:data)require(v==0,"Old reverse phrase replayed");}
        phrase.process(data.data(),480,0,0.7f,1.5f,false,1,0);require(!phrase.state(),"Stale reverse playback survived");
        phrase.process(data.data(),480,16,0.7f,1.5f,true,1,0);
        phrase.process(data.data(),480,16,0.7f,1.5f,true,1,1);require(!phrase.state(),"Reverse cancel failed");
        phrase.process(data.data(),480,16,0.7f,1.5f,true,1,1);require(!phrase.state(),"Reverse cancel rearmed held key");
        phrase.process(data.data(),480,0,0.7f,1.5f,true,1,1);
        for(unsigned i=0;i<1100;++i)phrase.process(data.data(),480,16,0.7f,1.5f,true,1,1);
        require(phrase.state()==11 && phrase.seconds()==10,"Reverse recording limit");
        phrase.process(data.data(),480,20,0.7f,1.5f,true,1,1);require(!phrase.state(),"Reverse/speed conflict failed");
        phrase.process(data.data(),480,16,0.7f,1.5f,true,1,1);require(!phrase.state(),"Reverse conflict rearmed held key");
        std::cout<<"reverse_sequence=passed live="<<reverseLive<<" delay_samples=7200 samples="<<recorded.size()<<'\n';
    }
    {
        // A sustained level exposes an abrupt end more clearly than a speech waveform.
        mic::PhraseEffect phrase;data.fill(0);phrase.process(data.data(),480,0,0.7f,1.5f,true,1,0);
        for(int f=0;f<20;++f){data.fill(0.8f);phrase.process(data.data(),480,16,0.7f,1.5f,true,1,0);}
        for(int f=0;f<15;++f){data.fill(0);phrase.process(data.data(),480,0,0.7f,1.5f,true,1,0);}
        float previous=0;
        for(int i=0;i<9600;++i){
            float v=0;phrase.process(&v,1,0,0.7f,1.5f,true,1,0);
            if(i>=6720)require(v<=previous && previous-v<0.0005f,"Reverse ending is abrupt or not monotonic");
            if(i==8160)require(v>0.39f && v<0.41f,"Reverse ending must taper across 60 ms");
            previous=v;
        }
        require(previous==0 && !phrase.state(),"Reverse must end at exact silence");
        for(int i=0;i<480;++i){float v=0.6f;phrase.process(&v,1,0,0.7f,1.5f,true,1,0);require(v>=previous && v-previous<0.002f,"Return to live voice jumps");previous=v;}
        require(std::abs(previous-0.6f)<1e-6,"Live voice did not recover");
        std::cout<<"reverse_soft_end=passed fade_ms=60 live_return_ms=10\n";
    }
    {
        mic::PhraseEffect phrase;data.fill(0);phrase.process(data.data(),480,0,0.7f,2,true,1,0);
        for(unsigned frame=0;frame<120;++frame){
            for(unsigned i=0;i<480;++i)data[i]=0.2f*std::sin(2*3.141592653589793*18000*(frame*480+i)/48000);
            phrase.process(data.data(),480,frame<100?8:0,0.7f,2,true,1,0);
        }
        float aliasPeak=0;
        for(unsigned i=0;i<28000;++i){float v=0;phrase.process(&v,1,0,0.7f,2,true,1,0);if(i>480)aliasPeak=std::max(aliasPeak,std::abs(v));}
        require(aliasPeak<0.003f,"Speed-up aliases high frequencies");
        std::cout<<"phrase_alias_peak="<<aliasPeak<<'\n';
    }
    {
        const auto checkTrim=[](const std::vector<float>& input,size_t begin,size_t end) {
            for(bool live:{true,false})for(size_t chunk:{size_t(17),size_t(480),size_t(997)}) {
                mic::PhraseEffect phrase;auto replay=std::make_unique<mic::LastEffect>();
                std::vector<float> output;std::array<float,997> buffer{};std::array<uint8_t,997> marks{};
                const auto process=[&](size_t n,unsigned held,unsigned request=0) {
                    std::fill(marks.begin(),marks.end(),0);
                    const bool active=phrase.state()!=0;bool discord=!live;
                    phrase.process(buffer.data(),n,held,0.7f,1.5f,true,1,0,live,marks.data());
                    replay->process(buffer.data(),n,marks.data(),discord,held,active||phrase.state()!=0,true,1,0,request);
                    require(discord==!live,"Trim changed replay source");
                };
                process(0,0); // Initialize replay epoch before capture.
                for(size_t at=0;at<input.size();) {
                    const size_t n=std::min(chunk,input.size()-at);
                    std::copy_n(input.data()+at,n,buffer.data());process(n,16);
                    for(size_t i=0;i<n;++i)require(buffer[i]==(live?input[at+i]:0),"Trim changed recording passthrough");
                    at+=n;
                }
                for(size_t at=0;at<7200;) {
                    const size_t n=std::min(chunk,7200-at);buffer.fill(0.8f);process(n,0);at+=n;
                    for(size_t i=0;i<n;++i)require(buffer[i]==0 && marks[i]==0,"Trim changed pause");
                    require(phrase.state()==(at==7200?12:10),"Trim changed delay length");
                }
                const size_t length=end-begin;
                require(std::abs(phrase.seconds()-length/48000.0)<1e-6,"Trim remaining time");
                for(size_t at=0;at<length;) {
                    const size_t n=std::min(chunk,length-at);buffer.fill(0);process(n,0);
                    for(size_t i=0;i<n;++i) {
                        const size_t p=at+i;
                        const double tail=std::clamp((length-p-1)/std::min(2880.0,length/2.0),0.0,1.0);
                        const float expected=static_cast<float>(input[end-1-p]*std::min(p/480.0,tail*tail*(3-2*tail)));
                        require(buffer[i]==expected && marks[i]==1,"Trim range, envelope or sample mismatch");
                        output.push_back(buffer[i]);
                    }
                    at+=n;
                }
                require(phrase.state()==0,"Trim did not finish at expected boundary");
                buffer.fill(0);process(480,0); // Close replay capture.
                for(size_t at=0;at<length;) {
                    const size_t n=std::min(chunk,length-at);buffer.fill(0);process(n,0,1);
                    for(size_t i=0;i<n;++i)require(buffer[i]==output[at+i],"Trim replay mismatch");
                    at+=n;
                }
            }
        };
        const auto clip=[](size_t leading,size_t middle,size_t trailing,float quiet=0) {
            std::vector<float> v(leading+middle+trailing,quiet);
            for(size_t i=leading;i<leading+middle;++i)v[i]=(i%2?0.2f:-0.2f);
            return v;
        };
        auto v=clip(14400,9600,24000);checkTrim(v,9600,28800);
        v=clip(14400,9600,0);checkTrim(v,9600,v.size());
        v=clip(0,9600,14400);checkTrim(v,0,14400);
        v=clip(9120,9600,9599);checkTrim(v,0,v.size());
        v=clip(9600,9600,9600);checkTrim(v,4800,24000);
        v=clip(14400,9601,14400);checkTrim(v,9600,29280); // Partial activity window retained.
        v=clip(14400,9600,9601);checkTrim(v,9600,28800); // Partial quiet window counts by samples.
        v=clip(14400,9600,14400,0.0001f);checkTrim(v,0,v.size()); // Equality is protected.
        v=clip(14400,9600,14400,0.000099f);checkTrim(v,9600,28800);
        v=clip(14400,9600,14400,0.000101f);checkTrim(v,0,v.size());
        v=clip(14400,9600,14400);for(auto& x:v)x*=0.01f;
        std::fill_n(v.begin(),14400,0.000003f);checkTrim(v,0,28800); // Relative threshold.
        v.assign(30000,0);checkTrim(v,0,v.size());
        v=clip(14400,9600,14400);for(auto& x:v)x*=0.005f;checkTrim(v,0,v.size());
        v=clip(9600,100,4000);checkTrim(v,0,v.size());
        v=clip(14400,9600,14400);v[0]=v.back()=0.00011f;checkTrim(v,0,v.size());
        v=clip(14400,19200,14400);std::fill(v.begin()+19200,v.begin()+28800,0);checkTrim(v,9600,38400);
        v=clip(14400,9600,14400);
        for(size_t i=24000;i<28800;++i)v[i]=0.00011f+0.001f*(28800-i)/4800;
        checkTrim(v,9600,33600); // Quiet ending stays outside the fade.
        v=clip(14400,480000-28800,14400);checkTrim(v,9600,470400);
        v.resize(500000,0.8f);checkTrim(v,9600,470400); // Post-limit audio cannot move boundaries.
        for(int phase:{0,1,2}) {
            mic::PhraseEffect phrase;std::vector<float> audio=clip(14400,9600,14400);
            phrase.process(audio.data(),audio.size(),16,0.7f,1.5f,true,1,0);
            float x=0;
            if(phase>0)for(int i=0;i<(phase==1?100:7300);++i)phrase.process(&x,1,0,0.7f,1.5f,true,1,0);
            phrase.process(&x,1,0,0.7f,1.5f,true,1,1);require(!phrase.state(),"Trim cancel state");
            audio.assign(19200,0.2f);phrase.process(audio.data(),audio.size(),16,0.7f,1.5f,true,1,1);
            for(int i=0;i<7200;++i){x=0;phrase.process(&x,1,0,0.7f,1.5f,true,1,1);}
            require(std::abs(phrase.seconds()-0.4f)<1e-6,"Trim metadata survived cancellation");
        }
        mic::PhraseEffect empty;float x=0;
        empty.process(&x,0,16,0.7f,1.5f,true,1,0);
        for(int i=0;i<7200;++i)empty.process(&x,1,0,0.7f,1.5f,true,1,0);
        require(!empty.state() && x==0,"Empty reverse range");
        std::cout<<"reverse_trim=passed sources=2 chunks=17,480,997 replay=passed\n";
    }
    {
        // Fixed-delay RVC playout without a server: the test plays the worker's role.
        // Small ring on purpose: effects_check has no enlarged stack.
        auto ring=std::make_unique<mic::Ring<8192,mic::RvcSample>>();
        mic::RvcPlayout playout;
        constexpr unsigned delay=1200,chunk=480;
        std::array<float,chunk> out{};std::array<uint8_t,chunk> marks{};
        uint64_t fed=0; // Stream index of the next "converted" sample; sample value = index+1, silence = 0.
        auto feed=[&](unsigned generation){
            std::array<mic::RvcSample,chunk> converted{};
            for(unsigned i=0;i<chunk;++i)converted[i]={static_cast<float>(fed+i+1),generation};
            fed+=chunk;return ring->push(converted.data(),chunk);
        };
        auto push=[&](unsigned generation){out.fill(0.5f);marks.fill(0);return playout.process(*ring,out.data(),chunk,marks.data(),delay,generation);};
        // Output position P = samples output before the call; out[i] must be value(P+i-delay) or 0.
        auto expect=[&](uint64_t position,unsigned from,unsigned to,bool audio){
            for(unsigned i=from;i<to;++i){
                const int64_t index=static_cast<int64_t>(position+i)-delay;
                require(out[i]==(audio?static_cast<float>(index+1):0.0f),"RVC playout sample mismatch");
                require(marks[i]==1,"RVC region must be marked as an effect, silence included");
            }
        };
        require(feed(1),"feed 0..479");
        require(push(1)==1,"priming state");expect(0,0,chunk,false);          // (1) exactly `delay` samples of silence,
        require(push(1)==1,"priming state 2");expect(480,0,chunk,false);      //     even with converted audio waiting
        require(push(1)==2,"delay boundary");expect(960,0,240,false);expect(960,240,chunk,true);
        require(playout.consumed==240,"consumed after boundary");
        require(feed(1),"feed 480..959");
        require(push(1)==2,"steady");expect(1440,0,chunk,true);              // (2) every sample delayed by exactly `delay`
        require(push(1)==3,"partial underrun");expect(1920,0,240,true);expect(1920,240,chunk,false);
        require(playout.consumed==960,"partial underrun consumption");
        require(push(1)==3,"full underrun");expect(2400,0,chunk,false);      // (3) nothing arrived: silence, no time shift
        require(playout.consumed==960,"underrun must not advance consumption");
        require(feed(1) && feed(1),"late chunks 960..1919 arrive together");
        require(push(1)==3,"late arrival");expect(2880,0,240,true);expect(2880,240,chunk,false);
        require(playout.consumed==1920,"overdue samples 960..1679 dropped, 1680..1919 played");
        require(feed(1) && feed(1),"feed 1920..2879");
        require(push(1)==2,"caught up");expect(3360,0,chunk,true);           // latency unchanged after the gap
        require(playout.consumed==2640,"consumption after catch-up");
        std::array<mic::RvcSample,100> stale{};for(auto& v:stale)v={9.0f,7}; // (4) foreign generation in the ring
        require(ring->push(stale.data(),100),"push stale samples");
        require(feed(1),"feed 2880..3359");
        require(push(1)==2,"across stale samples");expect(3840,0,chunk,true);
        require(playout.consumed==3120,"stale samples must not count");
        require(push(2)==1 && playout.pushed==chunk && playout.consumed==0,"generation change resets playout"); // (5)
        std::cout<<"rvc_playout=passed delay="<<delay<<" late_drop=passed stale_skip=passed\n";
    }
    std::cout<<"EFFECTS CHECK PASSED\n";return 0;
} catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}}
