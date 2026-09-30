/*
 * latency_rig.ino — measurement rig for the Latency Tester Suite
 * Board: Arduino Pro Micro (ATmega32U4, 5 V / 16 MHz).  In the Arduino IDE choose
 *        "Arduino Leonardo" (or "SparkFun Pro Micro 5V/16MHz").  Serial monitor: 115200 baud,
 *        "Newline" line ending.  Type h for help.
 *
 * WHAT IT DOES
 *   r  ROBOT (default at power-up).  A light sensor watches the white bar that the app shows when a
 *      stimulus starts.  The instant the bar turns white, OUT is driven high, which closes the mouse
 *      button (BC550) or fires the keyboard solenoid.  When the app has registered the input the bar
 *      changes again (red / black / red finish signal); the sketch times that and releases OUT.
 *      Every trial prints:  T,n,detect_to_out_us,out_to_contact_us,out_to_response_us
 *   c  CALIBRATE the robot's own delay (sensor + microcontroller + driver + mechanics up to the
 *      contact/reference switch).  Prints ROBOT_DELAY_MS — enter it in the app under "Rig calibration".
 *   d  DISPLAY pattern analysis: timestamps every light edge of the app's square-wave pattern and prints
 *      on/off durations, period, frequency (= refresh rate when the pattern flips every frame) and jitter.
 *   w / W  WAVEFORM: captures the analog light level around the next rising (w) or falling (W) edge and
 *      prints the 10%..90% rise/fall time of the display (response time).
 *   l  LDAT-style click-to-photon: sends F13 as a USB keyboard, the app (Display patterns ->
 *      "Flash when the Arduino sends F13") flashes white, the sensor sees it: prints trigger-to-light time.
 *
 * WIRING (Pro Micro pin labels; the code uses the AVR port bits directly, so the board package's pin
 * map does not matter):
 *   D4  (PD4/ICP1)  LIGHT     digital output of the light-sensor comparator, HIGH = white
 *   A0  (PF7/ADC7)  LIGHT_A   analog output of the photodiode amplifier, 0..5 V (waveform mode)
 *   D9  (PB5)       OUT       -> 1 kOhm -> BC550 base (mouse)   or   -> MOSFET gate driver (solenoid)
 *   D6  (PD7)       SENSE     contact feedback: mouse button node via 10 kOhm, or a reference switch to GND
 *   D10 (PB6)       CAL_LED   -> 330 Ohm -> LED aimed at the photodiode (calibration only)
 *   GND / VCC(5 V)
 * Full schematics and the parts list: docs/latency-rig/README.md
 */

#include <avr/io.h>
#include <avr/interrupt.h>
#include <EEPROM.h>
#include <Keyboard.h>
#include "rig_analysis.h"

// ------------------------------------------------------------------------------------------------
// Hardware access (direct port I/O: ~60 ns per operation)
// ------------------------------------------------------------------------------------------------
#define OUT_ON()    (PORTB |= _BV(PB5))
#define OUT_OFF()   (PORTB &= ~_BV(PB5))
#define LED_ON()    (PORTB |= _BV(PB6))
#define LED_OFF()   (PORTB &= ~_BV(PB6))
#define LIGHT_RAW() ((PIND & _BV(PD4)) != 0)
#define SENSE_LOW() ((PIND & _BV(PD7)) == 0)

// ------------------------------------------------------------------------------------------------
// Settings (stored in EEPROM with the 'S' command)
// ------------------------------------------------------------------------------------------------
struct Settings {
  uint32_t magic;
  uint16_t holdMinMs;    // keep OUT on at least this long (switch debounce in the device under test)
  uint16_t maxOnMs;      // never keep OUT on longer than this (protects the solenoid)
  uint16_t rearmMs;      // light must stay dark this long before the robot re-arms
  uint8_t  adcPrescaler; // 8, 16, 32, 64 or 128
  uint8_t  sensePullup;  // 1 = internal pull-up on SENSE (reference switch to GND), 0 = mouse node
  uint8_t  invertLight;  // 1 = comparator output is LOW for white
  uint16_t ldatCount;    // trials for the click-to-photon test
};
static const uint32_t SETTINGS_MAGIC = 0x4C415431UL;  // "LAT1"
static Settings cfg;

static void defaults() {
  cfg.magic = SETTINGS_MAGIC;
  cfg.holdMinMs = 12;
  cfg.maxOnMs = 300;
  cfg.rearmMs = 40;
  cfg.adcPrescaler = 32;
  cfg.sensePullup = 0;
  cfg.invertLight = 0;
  cfg.ldatCount = 20;
}

static void loadSettings() {
  EEPROM.get(0, cfg);
  if (cfg.magic != SETTINGS_MAGIC) defaults();
}

static void saveSettings() { EEPROM.put(0, cfg); }

static inline bool lightBright() { return LIGHT_RAW() != (cfg.invertLight != 0); }

// ------------------------------------------------------------------------------------------------
// 32-bit timestamp, 0.5 us resolution (Timer1 at 16 MHz / 8), wraps after ~35 minutes
// ------------------------------------------------------------------------------------------------
static volatile uint16_t t1Overflows = 0;
ISR(TIMER1_OVF_vect) { t1Overflows++; }

static inline uint32_t ticks32() {
  uint8_t sreg = SREG;
  cli();
  const uint16_t lo = TCNT1;
  const uint16_t hi = t1Overflows;
  const bool pending = (TIFR1 & _BV(TOV1)) != 0;   // wrapped but the ISR has not run yet
  SREG = sreg;
  return combineTicks(hi, lo, pending);
}

static inline uint32_t msToTicks(uint32_t ms) { return ms * 2000UL; }
static inline float ticksToUs(uint32_t t) { return t * 0.5f; }

// ------------------------------------------------------------------------------------------------
// Small helpers
// ------------------------------------------------------------------------------------------------
enum Mode { MODE_ROBOT, MODE_IDLE };
static Mode mode = MODE_ROBOT;
static uint32_t trialCount = 0;

static void applyPinConfig() {
  DDRB |= _BV(PB5) | _BV(PB6);       // OUT, CAL_LED outputs
  OUT_OFF();
  LED_OFF();
  DDRD &= ~(_BV(PD4) | _BV(PD7));    // LIGHT, SENSE inputs
  PORTD &= ~_BV(PD4);
  if (cfg.sensePullup) PORTD |= _BV(PD7); else PORTD &= ~_BV(PD7);
  DDRF &= ~_BV(PF7);                 // LIGHT_A analog input
}

static bool serialPending() { return Serial.available() > 0; }

static void flushInput() { while (Serial.available()) Serial.read(); }

static void printRunning(const __FlashStringHelper *label, const Running &s, const __FlashStringHelper *unit) {
  Serial.print(label);
  if (s.n == 0) { Serial.println(F(": no data")); return; }
  Serial.print(F(": n=")); Serial.print(s.n);
  Serial.print(F(" min=")); Serial.print(s.mn, 1);
  Serial.print(F(" mean=")); Serial.print(s.mean, 1);
  Serial.print(F(" max=")); Serial.print(s.mx, 1);
  Serial.print(F(" sd=")); Serial.print(s.sd(), 2);
  Serial.print(' '); Serial.println(unit);
}

// ------------------------------------------------------------------------------------------------
// ROBOT mode: white bar seen -> OUT on -> app reacts (bar leaves white) -> OUT off
// ------------------------------------------------------------------------------------------------
static const uint8_t SUMMARY_EVERY = 10;
static float sumDetOut = 0, sumContact = 0, sumResp = 0;
static uint16_t sumN = 0, sumContactN = 0, sumRespN = 0, misses = 0;

static void robotTrial() {
  // 1. the screen must have been dark (not white) for rearmMs before we arm; prevents firing on a
  //    bar that is still white from the previous trial
  uint32_t darkSince = ticks32();
  uint16_t spin = 0;
  for (;;) {
    if (lightBright()) darkSince = ticks32();
    else if (ticks32() - darkSince >= msToTicks(cfg.rearmMs)) break;
    if (++spin == 0 && serialPending()) return;
  }

  // 2. armed: wait for white.  This loop is the latency-critical part (a few CPU cycles per pass)
  spin = 0;
  while (!lightBright()) {
    if (++spin == 0 && serialPending()) return;
  }
  // Fire first, timestamp afterwards: the raw 16-bit counter reads (2 instructions each) bracket the
  // switch-on, so "detect -> out" is measured without delaying it.
  const uint16_t c0 = TCNT1;
  OUT_ON();
  const uint16_t c1 = TCNT1;
  const uint32_t tOut = ticks32();               // base for the later timings (~1.5 us after OUT rose)
  const uint16_t detectToOutTicks = (uint16_t)(c1 - c0);

  // 3. hold until the app responds (bar leaves white) and the minimum hold time has passed
  uint32_t tContact = 0, tResponse = 0;
  bool haveContact = false, haveResponse = false;
  uint8_t darkReads = 0;
  const uint32_t minRelease = tOut + msToTicks(cfg.holdMinMs);
  const uint32_t deadline = tOut + msToTicks(cfg.maxOnMs);
  for (;;) {
    const uint32_t now = ticks32();
    if (!haveContact && SENSE_LOW()) { tContact = now; haveContact = true; }
    if (!haveResponse) {
      if (!lightBright()) { if (++darkReads >= 3) { tResponse = now; haveResponse = true; } }
      else darkReads = 0;
    }
    if (now >= deadline) break;
    if (haveResponse && now >= minRelease) break;
  }
  OUT_OFF();

  // 4. report (printing happens after the timing-critical part)
  trialCount++;
  Serial.print(F("T,")); Serial.print(trialCount);
  Serial.print(','); Serial.print(detectToOutTicks * 0.5f, 1);
  Serial.print(',');
  if (haveContact) Serial.print(ticksToUs(tContact - tOut), 1); else Serial.print(F("NA"));
  Serial.print(',');
  if (haveResponse) Serial.print(ticksToUs(tResponse - tOut), 1); else { Serial.print(F("NA")); misses++; }
  Serial.println();

  sumDetOut += detectToOutTicks * 0.5f;
  if (haveContact) { sumContact += ticksToUs(tContact - tOut); sumContactN++; }
  if (haveResponse) { sumResp += ticksToUs(tResponse - tOut); sumRespN++; }
  if (++sumN >= SUMMARY_EVERY) {
    Serial.print(F("S,trials=")); Serial.print(sumN);
    Serial.print(F(",detect_to_out_us=")); Serial.print(sumDetOut / sumN, 1);
    Serial.print(F(",out_to_contact_us=")); if (sumContactN) Serial.print(sumContact / sumContactN, 1); else Serial.print(F("NA"));
    Serial.print(F(",out_to_response_ms=")); if (sumRespN) Serial.print(sumResp / sumRespN / 1000.0f, 3); else Serial.print(F("NA"));
    Serial.print(F(",no_response=")); Serial.println(misses);
    sumDetOut = sumContact = sumResp = 0; sumN = sumContactN = sumRespN = misses = 0;
  }
}

// ------------------------------------------------------------------------------------------------
// CALIBRATION: the robot's own delay with the calibration LED as a known-fast light source
// ------------------------------------------------------------------------------------------------
static uint16_t clip16(uint32_t t) { return t > 65535UL ? 65535U : (uint16_t)t; }

static void cmdCalibrate() {
  const uint8_t N = 40;
  static uint16_t ledDet[N], detOut[N], outContact[N];   // in 0.5 us ticks (max 32 ms)
  uint8_t got = 0, gotContact = 0;
  Serial.println(F("CAL: point the CAL LED (D10) at the photodiode. Mouse: SENSE on the button node. Keyboard: reference switch on SENSE."));
  for (uint8_t i = 0; i < N && !serialPending(); i++) {
    LED_OFF(); OUT_OFF();
    delay(30);
    if (lightBright()) { Serial.println(F("CAL: sensor reads bright with the LED off — shade it / adjust the threshold.")); return; }
    const uint32_t t0 = ticks32();
    LED_ON();
    uint32_t t1 = 0;
    while (ticks32() - t0 < msToTicks(20)) { if (lightBright()) { t1 = ticks32(); break; } }
    if (!t1) { LED_OFF(); Serial.println(F("CAL: sensor did not see the LED — check alignment / threshold.")); return; }
    OUT_ON();
    const uint32_t t2 = ticks32();
    uint32_t t3 = 0;
    while (ticks32() - t2 < msToTicks(cfg.maxOnMs)) { if (SENSE_LOW()) { t3 = ticks32(); break; } }
    OUT_OFF(); LED_OFF();
    ledDet[got] = clip16(t1 - t0);
    detOut[got] = clip16(t2 - t1);
    got++;
    if (t3) outContact[gotContact++] = clip16(t3 - t2);
    delay(60);
  }
  if (!got) { Serial.println(F("CAL: no data")); return; }
  const float a = medianTicksUs(ledDet, got), b = medianTicksUs(detOut, got);
  Serial.print(F("CAL led_to_detect_us=")); Serial.print(a, 1);
  Serial.print(F(" detect_to_out_us=")); Serial.print(b, 1);
  if (gotContact) {
    const float c = medianTicksUs(outContact, gotContact);
    Serial.print(F(" out_to_contact_us=")); Serial.println(c, 1);
    Serial.print(F("ROBOT_DELAY_MS=")); Serial.println((a + b + c) / 1000.0f, 3);
    Serial.println(F("Enter this value under Rig calibration (mouse robot / keyboard robot, depending on what is attached)."));
  } else {
    Serial.println(F(" out_to_contact_us=NA"));
    Serial.print(F("ROBOT_DELAY_MS(no contact seen; electronics only)=")); Serial.println((a + b) / 1000.0f, 3);
    Serial.println(F("SENSE never went low: check the contact wiring (u toggles the SENSE pull-up)."));
  }
}

// ------------------------------------------------------------------------------------------------
// DISPLAY pattern analysis: light edges of the app's square wave
// ------------------------------------------------------------------------------------------------
static void cmdEdges() {
  const uint16_t MAXE = 160;
  static uint32_t ev[MAXE];
  uint16_t n = 0;
  flushInput();
  bool prev = lightBright();
  const bool startBright = prev;
  Serial.println(F("EDGES: start the pattern in the app (Display / rig patterns -> Square wave). Send any key to stop."));
  const uint32_t startT = ticks32();
  while (n < MAXE && ticks32() - startT < msToTicks(30000UL) && !serialPending()) {
    const bool b = lightBright();
    if (b != prev) { ev[n++] = ticks32(); prev = b; }
  }
  flushInput();
  if (n < 4) { Serial.println(F("EDGES: fewer than 4 edges seen.")); return; }
  Running on, off, per;
  edgeStats(ev, n, startBright, on, off, per);
  Serial.print(F("EDGES: ")); Serial.print(n); Serial.println(F(" edges captured"));
  printRunning(F("white (on) time"), on, F("us"));
  printRunning(F("black (off) time"), off, F("us"));
  printRunning(F("period"), per, F("us"));
  if (per.n && per.mean > 0) {
    Serial.print(F("frequency = ")); Serial.print(1e6f / per.mean, 3); Serial.println(F(" Hz"));
    Serial.print(F("if the pattern flips every frame: refresh rate = ")); Serial.print(2e6f / per.mean, 2); Serial.println(F(" Hz"));
    Serial.print(F("period jitter (max-min) = ")); Serial.print(per.mx - per.mn, 1); Serial.println(F(" us"));
  }
}

// ------------------------------------------------------------------------------------------------
// WAVEFORM: analog light level around an edge -> 10..90% response time
// ------------------------------------------------------------------------------------------------
static uint8_t prescalerBits(uint8_t p) {
  switch (p) { case 8: return 3; case 16: return 4; case 32: return 5; case 64: return 6; default: return 7; }
}

static void cmdWaveform(bool risingEdge) {
  const uint16_t N = 384, PRE = 96;
  static uint8_t buf[N];
  ADMUX = _BV(REFS0) | _BV(ADLAR) | 7;        // AVcc reference, left adjust (8-bit in ADCH), ADC7 = A0
  ADCSRB &= ~_BV(MUX5);
  ADCSRA = _BV(ADEN) | _BV(ADSC) | _BV(ADATE) | prescalerBits(cfg.adcPrescaler);  // free running
  const float dtUs = 13.0f * cfg.adcPrescaler / 16.0f;   // 13 ADC clocks per conversion
  flushInput();
  Serial.print(F("WAVE: waiting for a ")); Serial.print(risingEdge ? F("rising") : F("falling"));
  Serial.print(F(" edge (black->white in the app pattern); sample interval ")); Serial.print(dtUs, 1); Serial.println(F(" us"));

  uint16_t idx = 0, quiet = 0, post = 0;
  uint32_t sampleNo = 0, trigNo = 0;
  bool triggered = false;
  const uint32_t startT = ticks32();
  while (true) {
    while (!(ADCSRA & _BV(ADIF))) { }
    ADCSRA |= _BV(ADIF);
    const uint8_t v = ADCH;
    buf[idx] = v; idx = (idx + 1) % N; sampleNo++;
    const bool bright = lightBright();
    if (!triggered) {
      if (bright != risingEdge) { if (quiet < 60000) quiet++; }
      else if (quiet >= PRE) { triggered = true; trigNo = sampleNo; post = 0; }
      else quiet = 0;
      if (((sampleNo & 0x3FF) == 0) && (serialPending() || ticks32() - startT > msToTicks(30000UL))) { ADCSRA = 0; flushInput(); Serial.println(F("WAVE: aborted / timeout")); return; }
    } else if (++post >= N - PRE) break;
  }
  ADCSRA = 0;

  // chronological access: the oldest sample is at idx
  #define W(i) (buf[(idx + (i)) % N])
  const int16_t trigPos = (int16_t)N - 1 - (int16_t)(sampleNo - trigNo);
  const WaveResult wr = analyzeWave(buf, N, idx);
  Serial.print(F("WAVE levels: before=")); Serial.print(wr.base, 1); Serial.print(F(" after=")); Serial.print(wr.fin, 1); Serial.println(F(" (0..255)"));
  if (wr.ok) {
    Serial.print(F("WAVE ")); Serial.print(risingEdge ? F("rise") : F("fall"));
    Serial.print(F(" time 10-90% = ")); Serial.print((wr.i90 - wr.i10) * dtUs, 0); Serial.println(F(" us"));
    Serial.print(F("WAVE 10% point to comparator trigger = ")); Serial.print((trigPos - wr.i10) * dtUs, 0); Serial.println(F(" us"));
  } else {
    Serial.println(F("WAVE: no clear transition in the analog signal (check A0 wiring / gain)."));
  }
  Serial.print(F("WAVE_DATA,dt_us=")); Serial.print(dtUs, 1); Serial.print(F(",trigger_index=")); Serial.print(trigPos);
  for (uint16_t i = 0; i < N; i++) { Serial.print(','); Serial.print(W(i)); }
  Serial.println();
  #undef W
}

// ------------------------------------------------------------------------------------------------
// LDAT-style click-to-photon: F13 over USB -> app flashes white -> light sensor
// ------------------------------------------------------------------------------------------------
static void cmdLdat(uint16_t count) {
  if (!count) count = cfg.ldatCount;
  if (count > 500) count = 500;
  Running res;
  uint16_t timeouts = 0;
  flushInput();
  Keyboard.begin();
  Serial.println(F("LDAT: focus the app window, choose Display / rig patterns -> 'Flash when the Arduino sends F13', press Start."));
  Serial.println(F("LDAT: starting in 3 s, send any key to stop."));
  delay(3000);
  for (uint16_t i = 0; i < count && !serialPending(); i++) {
    // the screen must be dark first
    uint32_t dark = ticks32();
    while (ticks32() - dark < msToTicks(150)) { if (lightBright()) dark = ticks32(); }
    delay(500 + (uint16_t)(random(0, 1000)));                    // random 0.5..1.5 s: unpredictable
    Keyboard.press(KEY_F13);
    const uint32_t t0 = ticks32();                               // report has been queued to the host
    uint32_t t1 = 0;
    while (ticks32() - t0 < msToTicks(500)) { if (lightBright()) { t1 = ticks32(); break; } }
    Keyboard.releaseAll();
    Serial.print(F("L,")); Serial.print(i + 1); Serial.print(',');
    if (t1) { const float ms = ticksToUs(t1 - t0) / 1000.0f; res.add(ms); Serial.println(ms, 3); }
    else { timeouts++; Serial.println(F("TIMEOUT")); }
    delay(200);
  }
  flushInput();
  printRunning(F("trigger->light"), res, F("ms"));
  Serial.print(F("timeouts: ")); Serial.println(timeouts);
  Serial.println(F("Note: includes the USB HID poll wait (0..1 ms, ~0.5 ms average) of the F13 report."));
}

// ------------------------------------------------------------------------------------------------
// Serial command interface
// ------------------------------------------------------------------------------------------------
static void printHelp() {
  Serial.println(F("latency_rig commands:"));
  Serial.println(F("  r        robot mode (default): white bar -> click/press, times the app's response"));
  Serial.println(F("  c        calibrate the robot's own delay (CAL LED aimed at the photodiode)"));
  Serial.println(F("  d        display edge analysis (square-wave pattern): on/off time, period, refresh rate"));
  Serial.println(F("  w / W    waveform of the next rising / falling edge: 10-90% response time"));
  Serial.println(F("  l [n]    click-to-photon test using F13 over USB (n trials)"));
  Serial.println(F("  s        show settings"));
  Serial.println(F("  H<ms>    minimum hold time of OUT"));
  Serial.println(F("  M<ms>    maximum time OUT may stay on (solenoid protection)"));
  Serial.println(F("  R<ms>    re-arm delay (sensor must be dark this long)"));
  Serial.println(F("  A<8|16|32|64|128>  ADC prescaler for waveform capture"));
  Serial.println(F("  u        toggle SENSE pull-up (on for a reference switch to GND, off for a mouse node)"));
  Serial.println(F("  i        toggle light polarity (if the comparator output is LOW for white)"));
  Serial.println(F("  S        save settings to EEPROM      D  restore defaults"));
}

static void printSettings() {
  Serial.print(F("holdMinMs=")); Serial.print(cfg.holdMinMs);
  Serial.print(F(" maxOnMs=")); Serial.print(cfg.maxOnMs);
  Serial.print(F(" rearmMs=")); Serial.print(cfg.rearmMs);
  Serial.print(F(" adcPrescaler=")); Serial.print(cfg.adcPrescaler);
  Serial.print(F(" sensePullup=")); Serial.print(cfg.sensePullup);
  Serial.print(F(" invertLight=")); Serial.print(cfg.invertLight);
  Serial.print(F(" light=")); Serial.print(lightBright() ? F("BRIGHT") : F("dark"));
  Serial.print(F(" sense=")); Serial.println(SENSE_LOW() ? F("closed") : F("open"));
}

static bool readLine(char *out, uint8_t cap) {
  uint8_t n = 0;
  const uint32_t t0 = millis();
  while (millis() - t0 < 50 && n < cap - 1) {
    if (Serial.available()) {
      const char c = Serial.read();
      if (c == '\n' || c == '\r') break;
      out[n++] = c;
    }
  }
  out[n] = 0;
  return n > 0;
}

static void handleSerial() {
  if (!serialPending()) return;
  char line[24];
  if (!readLine(line, sizeof line)) { return; }
  const char c = line[0];
  const long arg = atol(line + 1);
  switch (c) {
    case 'h': case '?': printHelp(); break;
    case 'r': mode = MODE_ROBOT; Serial.println(F("ROBOT mode armed")); break;
    case 'c': mode = MODE_IDLE; cmdCalibrate(); mode = MODE_ROBOT; break;
    case 'd': mode = MODE_IDLE; cmdEdges(); mode = MODE_ROBOT; break;
    case 'w': mode = MODE_IDLE; cmdWaveform(true); mode = MODE_ROBOT; break;
    case 'W': mode = MODE_IDLE; cmdWaveform(false); mode = MODE_ROBOT; break;
    case 'l': mode = MODE_IDLE; cmdLdat((uint16_t)arg); mode = MODE_ROBOT; break;
    case 's': printSettings(); break;
    case 'H': if (arg >= 0 && arg <= 1000) { cfg.holdMinMs = arg; printSettings(); } break;
    case 'M': if (arg >= 1 && arg <= 2000) { cfg.maxOnMs = arg; printSettings(); } break;
    case 'R': if (arg >= 1 && arg <= 2000) { cfg.rearmMs = arg; printSettings(); } break;
    case 'A': if (arg == 8 || arg == 16 || arg == 32 || arg == 64 || arg == 128) { cfg.adcPrescaler = arg; printSettings(); } break;
    case 'u': cfg.sensePullup ^= 1; applyPinConfig(); printSettings(); break;
    case 'i': cfg.invertLight ^= 1; printSettings(); break;
    case 'S': saveSettings(); Serial.println(F("saved")); break;
    case 'D': defaults(); applyPinConfig(); printSettings(); break;
    default: Serial.println(F("unknown command, h for help")); break;
  }
  flushInput();
}

// ------------------------------------------------------------------------------------------------
void setup() {
  loadSettings();
  applyPinConfig();
  TCCR1A = 0;
  TCCR1B = _BV(CS11);      // clk/8 = 2 MHz
  TIMSK1 = _BV(TOIE1);
  TCNT1 = 0;
  Serial.begin(115200);    // USB CDC: the baud rate is ignored, and we never wait for a terminal
  randomSeed(analogRead(A0) ^ (uint16_t)micros());
  delay(500);
  Serial.println(F("latency_rig ready. Robot mode armed. Type h for help."));
  printSettings();
}

void loop() {
  handleSerial();
  if (mode == MODE_ROBOT) robotTrial();
}
