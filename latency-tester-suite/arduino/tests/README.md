# Firmware tests (no hardware needed)

## Host test of the analysis maths

```bash
g++ -std=c++11 -Wall -o /tmp/t arduino/tests/test_analysis.cpp && /tmp/t
# prints: all rig_analysis tests passed
```

## Firmware in the simavr simulator

Needs `avr-gcc`, the Arduino Leonardo core (from the Arduino AVR boards package) and `simavr`
(`libsimavr-dev`, `libelf-dev`).

```bash
# 1. compile the sketch (all core .c/.cpp files + latency_rig.ino as C++) for atmega32u4 at 16 MHz:
#    -mmcu=atmega32u4 -DF_CPU=16000000UL -DARDUINO=10819 -DUSB_VID=0x2341 -DUSB_PID=0x8036 -Os
#    include paths: <core>/cores/arduino, <core>/variants/leonardo, <core>/libraries/{EEPROM,HID,Keyboard}/src
#    link into rig.elf
# 2. run the simulated robot against it:
gcc -O2 -o sim_robot arduino/tests/sim_robot.c -lsimavr -lelf && ./sim_robot rig.elf
```

The simulator drives the LIGHT pin, watches OUT and the SENSE pin, and checks that the robot fires within a few
microseconds, releases correctly, and that calibration reports the expected delay.
