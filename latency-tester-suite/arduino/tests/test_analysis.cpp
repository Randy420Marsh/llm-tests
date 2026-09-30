// Host-side tests for rig_analysis.h:   g++ -std=c++11 -Wall -o /tmp/t tests/test_analysis.cpp && /tmp/t
#include <cassert>
#include <initializer_list>
#include <cstdio>
#include <cmath>
#include "../latency_rig/rig_analysis.h"

static bool near(float a, float b, float eps = 1e-3f) { return fabsf(a - b) <= eps; }

static void test_running() {
  Running r;
  assert(r.n == 0 && r.sd() == 0);
  for (float x : {10.f, 20.f, 30.f, 40.f}) r.add(x);
  assert(r.n == 4 && near(r.mean, 25) && near(r.mn, 10) && near(r.mx, 40));
  assert(near(r.sd(), 12.9099f, 1e-2f));
}

static void test_ticks() {
  assert(combineTicks(0, 100, false) == 100);
  assert(combineTicks(3, 0x1234, false) == (3UL << 16 | 0x1234));
  // counter just wrapped, ISR pending: the high word must already include the overflow
  assert(combineTicks(5, 10, true) == (6UL << 16 | 10));
  // flag set while the counter is high: the flag belongs to the next wrap, not this reading
  assert(combineTicks(5, 0xFFF0, true) == (5UL << 16 | 0xFFF0));
  // monotonic across a wrap
  uint32_t a = combineTicks(0, 0xFFFF, false), b = combineTicks(0, 1, true);
  assert(b > a);
}

static void test_median() {
  uint16_t v[] = {30, 10, 20};                // ticks -> 0.5 us each
  assert(near(medianTicksUs(v, 3), 10.0f));   // median 20 ticks = 10 us
  uint16_t w[] = {4, 2, 8, 6};
  assert(near(medianTicksUs(w, 4), 2.5f));    // (4+6)/2 = 5 ticks = 2.5 us
  assert(medianTicksUs(w, 0) == 0);
  uint16_t outlier[] = {20, 22, 21, 60000, 19};
  assert(near(medianTicksUs(outlier, 5), 10.5f));   // USB-interrupt outlier ignored
}

static void test_edges() {
  // 60 Hz refresh, pattern flips every frame -> period 33333 us; edges every 16666.5 us
  uint32_t ev[41];
  for (int i = 0; i < 41; i++) ev[i] = (uint32_t)(i * 33333);   // ticks: 16666.5 us
  Running on, off, per;
  edgeStats(ev, 41, false, on, off, per);                       // starts dark: edge 0 is rising
  assert(on.n == 20 && off.n == 20);
  assert(near(on.mean, 16666.5f, 0.1f) && near(off.mean, 16666.5f, 0.1f));
  assert(per.n == 20 && near(per.mean, 33333.f, 0.1f));
  assert(near(2e6f / per.mean, 60.0f, 0.01f));                  // refresh rate formula used by the sketch

  // starts bright: first edge is falling
  Running on2, off2, per2;
  uint32_t e2[] = {0, 100, 300, 400, 600};                      // fall, rise, fall, rise, fall
  edgeStats(e2, 5, true, on2, off2, per2);
  assert(off2.n == 2 && on2.n == 2);
  assert(near(on2.mean, 100.0f) && near(off2.mean, 50.0f));     // bright 200 ticks = 100 us, dark 100 ticks = 50 us
  assert(per2.n == 1 && near(per2.mean, 150.0f));
}

static void test_wave() {
  const uint16_t N = 384;
  uint8_t buf[N];
  // dark (10) then a linear 10 -> 210 ramp over 40 samples starting at sample 150, then bright
  for (uint16_t i = 0; i < N; i++) {
    float v = i < 150 ? 10 : (i < 190 ? 10 + (i - 150) * 5.0f : 210);
    buf[i] = (uint8_t)v;
  }
  WaveResult r = analyzeWave(buf, N, 0);
  assert(r.ok && near(r.base, 10) && near(r.fin, 210));
  // 10% = 30 -> sample 154; 90% = 190 -> sample 186; 32 samples between
  assert(r.i10 == 154 && r.i90 == 186);

  // rotated ring buffer: same data, oldest sample stored at index 100
  uint8_t ring[N];
  for (uint16_t i = 0; i < N; i++) ring[(100 + i) % N] = buf[i];
  WaveResult r2 = analyzeWave(ring, N, 100);
  assert(r2.ok && r2.i10 == r.i10 && r2.i90 == r.i90);

  // falling edge
  uint8_t fall[N];
  for (uint16_t i = 0; i < N; i++) fall[i] = 255 - buf[i];
  WaveResult r3 = analyzeWave(fall, N, 0);
  assert(r3.ok && r3.i90 - r3.i10 == 32);

  // flat signal: no transition
  uint8_t flat[N];
  for (uint16_t i = 0; i < N; i++) flat[i] = 100;
  assert(!analyzeWave(flat, N, 0).ok);
}

int main() {
  test_running();
  test_ticks();
  test_median();
  test_edges();
  test_wave();
  puts("all rig_analysis tests passed");
  return 0;
}
