#pragma once
#include <cstdint>
#include <cstddef>
namespace mic {
// Existing bindings keep bits 0-9. New microphone effects use 10-13 and Discord 14-17.
enum Hold : unsigned {
    HoldBoost=1, HoldPitch=2, HoldSlow=4, HoldFast=8, HoldReverse=16,
    HoldEcho=1<<10, HoldStutter=1<<11, HoldAutoTune=1<<13, // Bits 12/16 retired (Granular).
    HoldNew=HoldEcho|HoldStutter|HoldAutoTune,
    HoldPhrases=HoldSlow|HoldFast|HoldReverse, HoldLive=HoldBoost|HoldPitch|HoldNew,
    DiscordShift=5, HoldMicMask=31|HoldNew, HoldAllMask=((1<<18)-1)&~((1<<12)|(1<<16))
};
inline unsigned sourceHeld(unsigned flags,bool discord) {
    return discord?((flags>>DiscordShift)&31)|((flags>>4)&HoldNew):flags&HoldMicMask;
}
inline unsigned sourceRecordFlags(unsigned selected,bool discord) {
    return discord?((selected&31)<<DiscordShift)|((selected&HoldNew)<<4):selected;
}
// Per-sample effect categories carried through the output queues for effects-only monitoring.
enum Modified : uint8_t { ModifiedEffects=1, ModifiedBoost=2, ModifiedSound=4 }; // ModifiedSound: monitor mask bit; set only on preview-queue samples
inline uint64_t packHeld(uint64_t now,unsigned epoch,unsigned flags,bool eligible=true) {
    return ((now&((1ull<<29)-1))<<35)|((static_cast<uint64_t>(epoch)&65535)<<19)|(eligible?1ull<<18:0)|(flags&HoldAllMask);
}
inline bool heldFresh(uint64_t sample,unsigned epoch,uint64_t now) {
    return (sample&(1ull<<18)) && ((sample>>19)&65535)==(epoch&65535)
        && ((now-(sample>>35))&((1ull<<29)-1))<=250;
}
inline unsigned heldFlags(uint64_t sample,unsigned epoch,uint64_t now,bool eligible) {
    return eligible && heldFresh(sample,epoch,now)?static_cast<unsigned>(sample&HoldAllMask):0;
}
inline float heldIntensity(float normal,float alternate,uint64_t sample,unsigned epoch,uint64_t now,bool eligible) {
    return (heldFlags(sample,epoch,now,eligible)&1)?alternate:normal;
}
struct HoldLatch {
    bool armed[18]{};
    unsigned epoch=0;
    template<size_t N> unsigned update(unsigned currentEpoch,bool eligible,const unsigned (&keys)[N],const bool (&pressed)[N],unsigned mods,bool win,bool discordReady=true) {
        static_assert(N<=18);
        if(epoch!=currentEpoch){epoch=currentEpoch;for(auto& a:armed)a=false;}
        unsigned flags=0;
        for(unsigned i=0;i<N;++i) {
            const bool allowed=eligible && (i<5 || (i>=10 && i<14) || discordReady);
            if(!allowed) armed[i]=false;
            else if(!pressed[i]) armed[i]=true;
            if(allowed && armed[i] && keys[i] && pressed[i] && mods==(keys[i]>>8) && !win) flags|=1<<i;
        }
        return flags&HoldAllMask;
    }
};
}
