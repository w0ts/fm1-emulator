// SPDX-License-Identifier: GPL-3.0-only
// The BLE engine model and the virtual central against unchanged stock V15
// (external, like tests/stock.rs). Run in release mode with --ignored and
// FM1_STOCK_FWSC=/path/to/FM-1.fwsc.
use fm1_emu::ble_central::{script, Central, CentralConfig, Step, MIDI_SERVICE};
use fm1_emu::{cpu::Cpu, firmware::Firmware};
use std::{env, path::Path};

fn stock() -> Cpu {
    let path = env::var("FM1_STOCK_FWSC").expect("set FM1_STOCK_FWSC to unchanged FM-1.fwsc");
    let firmware = Firmware::load(Path::new(&path)).unwrap();
    let mut cpu = Cpu::new(firmware.bus().unwrap(), firmware.entry);
    cpu.r[0] = 0x01c7_fe08;
    cpu
}

fn run_until(cpu: &mut Cpu, limit: u64, mut done: impl FnMut(&Cpu) -> bool) {
    let mut ran = 0;
    while ran < limit && !done(cpu) {
        cpu.run_steps(1_000_000)
            .unwrap_or_else(|e| panic!("after {} instructions: {e}", cpu.steps));
        ran += 1_000_000;
    }
}

#[test]
#[ignore = "requires FM1_STOCK_FWSC (official FM-1_015); run in release mode with --ignored"]
fn stock_v15_advertises_fm1_ble_with_the_midi_service_and_answers_scans() {
    let mut cpu = stock();
    cpu.bus.ble_attach_central(Central::new(
        CentralConfig::default(),
        vec![Step::Scan(u32::MAX)],
    ));
    run_until(&mut cpu, 400_000_000, |cpu| {
        cpu.bus.ble_central().unwrap().scan_responses().len() >= 2
    });
    let central = cpu.bus.ble_central().unwrap();
    let adv = &central.adv_reports()[0];
    // Feasibility doc §9.2: flags, the BLE-MIDI service UUID, "sinco".
    let mut expected = vec![0x02, 0x01, 0x06, 0x11, 0x07];
    expected.extend(MIDI_SERVICE);
    expected.extend([0x06, 0xFF, b's', b'i', b'n', b'c', b'o']);
    assert_eq!(adv.kind, 0, "ADV_IND");
    assert!(!adv.random, "public address (TxAdd 0)");
    assert_eq!(adv.data, expected);
    let channels: Vec<_> = central
        .adv_reports()
        .iter()
        .take(3)
        .map(|r| r.channel)
        .collect();
    assert_eq!(channels, [37, 38, 39]);
    let rsp = &central.scan_responses()[0];
    assert_eq!(rsp.address, adv.address);
    assert_eq!(rsp.data, b"\x09\x09FM-1_BLE");
    let engine = cpu.bus.ble_engine();
    assert_eq!(engine.link_state(0), 2);
    eprintln!("stock V15: {:?}", engine.stats);
}

#[test]
#[ignore = "requires FM1_STOCK_FWSC; about 1.5e9 instructions in release mode"]
fn stock_v15_connects_discovers_gatt_and_exchanges_ble_midi() {
    let mut cpu = stock();
    // Experiment stub: the unmodelled block at 0x41200 that stock uses right
    // after the connection set-up (status bits 2 and 5 read as done).
    cpu.bus
        .set_mmio_stubs_clearing(vec![(0x41200, 0x412ff, 0x24, 0)]);
    let config = CentralConfig {
        answer_smp: false,
        ..Default::default()
    };
    let steps = script::parse("scan 2\nconnect\nversion\nfeatures\nmtu 527\ndiscover\nread\nsubscribe\nmidi 80 80 90 3c 64\nwait 30\n")
        .unwrap();
    cpu.bus.ble_attach_central(Central::new(config, steps));
    run_until(&mut cpu, 2_000_000_000, |cpu| {
        cpu.bus.ble_central().unwrap().finished()
    });
    let central = cpu.bus.ble_central().unwrap();
    for r in central.results() {
        assert!(r.outcome.is_ok(), "{:?}: {:?}", r.step, r.outcome);
    }
    assert!(central.finished());
    assert_eq!(central.att_mtu(), 517);
    let midi = central
        .characteristics()
        .iter()
        .find(|c| c.value == 0x72)
        .expect("BLE-MIDI characteristic at 0x72 (§9.2)");
    assert_eq!(midi.properties, 0x16);
    assert!(central
        .services()
        .iter()
        .any(|s| s.uuid.0 == MIDI_SERVICE && s.start == 0x70));
    // The BLE-MIDI note on reached the synth.
    assert!(
        cpu.bus.audio.samples.iter().any(|frame| *frame != [0, 0]),
        "the BLE-MIDI note must render audio"
    );
    // Pressing a note key sends a BLE-MIDI notification ("80 80" header).
    cpu.bus.devices.gpio.press(3, 4, true).unwrap();
    run_until(&mut cpu, 200_000_000, |cpu| {
        !cpu.bus.ble_central().unwrap().notifications().is_empty()
    });
    let n = &cpu.bus.ble_central().unwrap().notifications()[0];
    assert_eq!(n.handle, 0x72);
    assert_eq!(n.value[..3], [0x80, 0x80, 0x90]);
}
