// rig_analysis.h — pure measurement maths for latency_rig.ino.
// No Arduino dependencies, so the same code is unit-tested on a PC (arduino/tests/test_analysis.cpp).
#ifndef RIG_ANALYSIS_H
#define RIG_ANALYSIS_H

#include <stdint.h>
#include <math.h>

// Streaming statistics (Welford): no sample arrays, because the ATmega32U4 has only 2.5 KB of SRAM
struct Running {
  uint16_t n = 0;
  float mean = 0, m2 = 0, mn = 0, mx = 0;
  void add(float x) {
    if (n == 0) mn = mx = x; else { if (x < mn) mn = x; if (x > mx) mx = x; }
    n++;
    const float d = x - mean;
    mean += d / n;
    m2 += d * (x - mean);
  }
  float sd() const { return n > 1 ? sqrtf(m2 / (n - 1)) : 0; }
};

// 32-bit timestamp from the Timer1 overflow counter and the counter value. `overflowPending` is the
// TOV1 flag: the counter wrapped but the interrupt has not incremented the counter yet.
static inline uint32_t combineTicks(uint16_t overflows, uint16_t counter, bool overflowPending) {
  uint16_t hi = overflows;
  if (overflowPending && counter < 0x8000) hi++;
  return ((uint32_t)hi << 16) | counter;
}

// Median of up to 64 values held as 0.5 us ticks (sorts in place); result in microseconds
static inline float medianTicksUs(uint16_t *v, uint8_t n) {
  for (uint8_t i = 1; i < n; i++) {
    uint16_t x = v[i];
    int8_t j = i - 1;
    while (j >= 0 && v[j] > x) { v[j + 1] = v[j]; j--; }
    v[j + 1] = x;
  }
  if (!n) return 0;
  const float t = (n & 1) ? v[n / 2] : 0.5f * (v[n / 2 - 1] + v[n / 2]);
  return t * 0.5f;
}

// Timestamps (0.5 us ticks) of alternating light edges -> on time, off time and period statistics.
// `startBright` is the light level before the first edge.
static inline void edgeStats(const uint32_t *ev, uint16_t n, bool startBright, Running &on, Running &off, Running &per) {
  auto rising = [&](uint16_t i) { return startBright ? (i % 2 == 1) : (i % 2 == 0); };
  for (uint16_t i = 0; i + 1 < n; i++) {
    const float d = (ev[i + 1] - ev[i]) * 0.5f;
    if (rising(i)) on.add(d); else off.add(d);
    if (rising(i) && i + 2 < n) {
      for (uint16_t j = i + 2; j < n; j += 2) {
        if (rising(j)) { per.add((ev[j] - ev[i]) * 0.5f); break; }
      }
    }
  }
}

struct WaveResult {
  bool ok;
  float base, fin;       // levels before / after the edge (0..255)
  int16_t i10, i90;      // sample index of the 10% / 90% crossing
};

// Ring buffer `buf` of N samples, oldest at `idx`. Levels are averaged over the first/last 16 samples.
static inline WaveResult analyzeWave(const uint8_t *buf, uint16_t N, uint16_t idx) {
  WaveResult r = {false, 0, 0, -1, -1};
  #define WAVE_AT(i) (buf[(idx + (i)) % N])
  for (uint8_t i = 0; i < 16; i++) { r.base += WAVE_AT(i); r.fin += WAVE_AT(N - 1 - i); }
  r.base /= 16; r.fin /= 16;
  const float span = r.fin - r.base;
  if ((span < 0 ? -span : span) < 20) return r;   // no clear transition
  const float lo = r.base + 0.1f * span, hi = r.base + 0.9f * span;
  for (uint16_t i = 0; i < N; i++) {
    const bool past10 = span > 0 ? WAVE_AT(i) >= lo : WAVE_AT(i) <= lo;
    const bool past90 = span > 0 ? WAVE_AT(i) >= hi : WAVE_AT(i) <= hi;
    if (r.i10 < 0 && past10) r.i10 = i;
    if (r.i90 < 0 && past90) { r.i90 = i; break; }
  }
  #undef WAVE_AT
  r.ok = r.i10 >= 0 && r.i90 >= 0;
  return r;
}

#endif
