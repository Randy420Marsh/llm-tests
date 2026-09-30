/*
 * Runs the compiled latency_rig firmware in the simavr AVR simulator and checks the robot timing.
 *
 *   build the firmware (see arduino/tests/README.md), then:
 *   gcc -O2 -o sim_robot sim_robot.c -lsimavr -lelf && ./sim_robot rig.elf
 *
 * The harness plays the roles of the light sensor (PD4), the contact feedback (PD7) and the app:
 * it raises the light input at a chosen time and checks how many microseconds later the firmware
 * drives OUT (PB5), and that it releases it when the light drops again.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <simavr/sim_avr.h>
#include <simavr/sim_elf.h>
#include <simavr/avr_ioport.h>

#define F_CPU_HZ 16000000ULL
#define US(c) ((double)(c) / 16.0)
#define MS_CYCLES(ms) ((uint64_t)(ms) * 16000ULL)
#define PLLCSR_ADDR 0x49   /* I/O address; data space = +0x20 */

static avr_t *avr;
static uint64_t out_rise = 0, out_fall = 0;
static int out_state = 0;

static void out_notify(struct avr_irq_t *irq, uint32_t value, void *param) {
  (void)irq; (void)param;
  if (value && !out_state) out_rise = avr->cycle;
  if (!value && out_state) out_fall = avr->cycle;
  out_state = value ? 1 : 0;
}

static void set_pin(char port, int bit, int v) {
  avr_irq_t *irq = avr_io_getirq(avr, AVR_IOCTL_IOPORT_GETIRQ(port), bit);
  avr_raise_irq(irq, v);
}

static void run_until(uint64_t cyc) {
  while (avr->cycle < cyc) {
    avr->data[PLLCSR_ADDR + 0x20] |= 1;   /* pretend the USB PLL locked */
    int st = avr_run(avr);
    if (st == cpu_Done || st == cpu_Crashed) { fprintf(stderr, "cpu stopped (%d)\n", st); exit(2); }
  }
}

static int fails = 0;
#define CHECK(cond, ...) do { if (!(cond)) { printf("FAIL: "); printf(__VA_ARGS__); printf("\n"); fails++; } else { printf("ok:   "); printf(__VA_ARGS__); printf("\n"); } } while (0)

int main(int argc, char **argv) {
  const char *path = argc > 1 ? argv[1] : "rig.elf";
  elf_firmware_t fw;
  memset(&fw, 0, sizeof fw);
  if (elf_read_firmware(path, &fw)) { fprintf(stderr, "cannot read %s\n", path); return 1; }
  fw.frequency = F_CPU_HZ;
  strcpy(fw.mmcu, "atmega32u4");
  avr = avr_make_mcu_by_name("atmega32u4");
  if (!avr) { fprintf(stderr, "simavr has no atmega32u4 core\n"); return 1; }
  avr_init(avr);
  avr_load_firmware(avr, &fw);
  avr->log = 1;
  avr_irq_register_notify(avr_io_getirq(avr, AVR_IOCTL_IOPORT_GETIRQ('B'), 5), out_notify, NULL);

  /* light sensor idle: dark; SENSE (PD7) idle high */
  set_pin('D', 4, 0);
  set_pin('D', 7, 1);
  run_until(MS_CYCLES(1200));   /* setup() has a 500 ms delay, then the robot arms after 40 ms of darkness */
  CHECK(out_state == 0, "OUT stays low while the sensor is dark");

  /* ---- trial 1: white bar appears; mechanical contact 250 us later; the app answers after 25 ms ---- */
  uint64_t t_light = avr->cycle;
  set_pin('D', 4, 1);
  uint64_t t_contact = 0;
  int contact_done = 0, light_dropped = 0;
  uint64_t t_drop = t_light + MS_CYCLES(25);
  while (avr->cycle < t_light + MS_CYCLES(80)) {
    run_until(avr->cycle + 4);
    if (out_state && !contact_done && avr->cycle >= out_rise + 250 * 16) { set_pin('D', 7, 0); contact_done = 1; t_contact = avr->cycle; }
    if (!light_dropped && avr->cycle >= t_drop) { set_pin('D', 4, 0); light_dropped = 1; }
  }
  CHECK(out_rise > 0, "OUT went high after the light appeared");
  double det = US(out_rise - t_light);
  CHECK(det < 10.0, "light -> OUT latency is %.2f us (< 10 us)", det);
  CHECK(out_fall > out_rise, "OUT was released");
  double held = US(out_fall - out_rise) / 1000.0;
  CHECK(held > 24.0 && held < 27.0, "OUT held %.2f ms (until the app answered at ~25 ms)", held);
  (void)t_contact;

  /* ---- trial 2: fast app response (2 ms) must still hold OUT for the minimum hold time (12 ms) ---- */
  set_pin('D', 7, 1);
  run_until(avr->cycle + MS_CYCLES(100));   /* re-arm delay */
  out_rise = out_fall = 0;
  t_light = avr->cycle;
  set_pin('D', 4, 1);
  light_dropped = 0;
  t_drop = t_light + MS_CYCLES(2);
  while (avr->cycle < t_light + MS_CYCLES(60)) {
    run_until(avr->cycle + 4);
    if (!light_dropped && avr->cycle >= t_drop) { set_pin('D', 4, 0); light_dropped = 1; }
  }
  held = US(out_fall - out_rise) / 1000.0;
  CHECK(out_rise > 0 && held >= 11.9 && held < 14.0, "fast response: OUT held %.2f ms (minimum hold 12 ms)", held);

  /* ---- trial 3: app never answers -> OUT is released at maxOnMs (300 ms) ---- */
  run_until(avr->cycle + MS_CYCLES(100));
  out_rise = out_fall = 0;
  t_light = avr->cycle;
  set_pin('D', 4, 1);
  run_until(t_light + MS_CYCLES(400));
  held = US(out_fall - out_rise) / 1000.0;
  CHECK(out_rise > 0 && held > 299.0 && held < 302.0, "no response: OUT released by the safety timeout after %.1f ms", held);

  /* ---- a bar that is still white at start-up must not fire until it has been dark ---- */
  set_pin('D', 4, 0);
  run_until(avr->cycle + MS_CYCLES(20));   /* dark for only 20 ms (< 40 ms re-arm) */
  out_rise = 0;
  set_pin('D', 4, 1);
  run_until(avr->cycle + MS_CYCLES(5));
  CHECK(out_rise == 0, "does not fire before the re-arm time has passed");

  printf(fails ? "\n%d check(s) FAILED\n" : "\nall robot simulation checks passed\n", fails);
  return fails ? 1 : 0;
}
