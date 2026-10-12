# Disclaimer

## Not affiliated with WHOOP

`whoop-rs` is an independent, unofficial project. It is not produced,
endorsed, supported or reviewed by WHOOP, Inc. "WHOOP" is a trademark of its
owner and is used here only to identify the hardware this software
interoperates with.

## Not a medical device

Nothing this software produces is a clinical measurement, a diagnosis, or a
basis for a medical decision. It decodes values a strap transmits; it does not
validate them.

That matters more here than the phrase usually implies, because several
quantities look clinical and are not:

- **ECG samples** are signed integers with **no established volts per count**.
  The waveform is not calibrated, its absolute sample timing is unresolved, and
  the quality and classifier codes in its status region have no established
  vocabulary. They are not a bad/good/excellent scale.
- **SpO₂-adjacent fields** on WHOOP 4 are raw ADC words. They are not
  percentages, and no conversion to one is documented.
- **Skin temperature** uses a documented `raw/100 °C` convention on WHOOP 5
  only. The WHOOP 4 field is a raw ADC word with no documented conversion, so
  this crate reports no temperature for it rather than borrowing the other
  generation's scale.
- **Respiratory-rate and signal-quality** fields are raw words, not breaths per
  minute or a quality percentage.
- **Accelerometer and gyroscope scales** (`1/4096` g, `2000/32768` °/s,
  `1/16384` g) are *applied interpretations* that matched physical cross-checks
  in the observations they came from. They are not confirmed device parameters.
- **Body location** is a diagnostic read. It never feeds wear detection, sleep
  staging or scoring, and should not.

If a field's meaning is unresolved, this crate reports it raw and says so
rather than converting it into a unit it has not earned.

## Interoperability

The wire contracts implemented here — service and characteristic UUIDs,
checksum parameters, frame layouts, command numbers and field offsets — are
recorded as factual observations for the purpose of interoperating with
hardware the user owns. No proprietary source code is reproduced, and no
circumvention of any protection measure is involved or required: the interfaces
described are those a strap exposes over standard Bluetooth Low Energy to any
connected peer.

## Risk

This software writes to hardware you own. Commands that can discard
unoffloaded history, write firmware or enter a bootloader are refused before a
frame is built, and configuration writes and ECG commands require explicit
opt-in — but the remaining surface still changes strap state:

- arming or disabling alarms replaces stored schedules
- `RUN_ALARM` clears the slot it fires
- renaming a WHOOP 4 strap applies on reboot, interrupting anything in flight
- the restart probe restarts the strap
- acknowledging an offload chunk lets the strap reclaim that history

Provided as-is, without warranty of any kind, under the terms of the
[Apache License 2.0](LICENSE). You are responsible for what you send to your
own hardware.
