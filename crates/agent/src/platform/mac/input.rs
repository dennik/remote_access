//! Injects the viewer's mouse and keyboard events with CGEvent.
//!
//! macOS needs more state than Windows: moves while a button is held must be
//! drag events, double-clicks need an explicit click count, and modifier flags
//! have to be set on every event for shortcuts like Cmd+C to register.

use crate::keymap::{self, MacKey};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use rd_common::proto::{MonitorInfo, MouseButton};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: f64 = 4.0;

struct State {
    position: CGPoint,
    buttons: Vec<MouseButton>,
    modifiers: Vec<MacKey>,
    last_click: Option<(MouseButton, Instant, CGPoint)>,
    click_count: i64,
}

static STATE: Mutex<State> = Mutex::new(State {
    position: CGPoint { x: 0.0, y: 0.0 },
    buttons: Vec::new(),
    modifiers: Vec::new(),
    last_click: None,
    click_count: 1,
});

fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
}

fn flags(modifiers: &[MacKey]) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for key in modifiers {
        flags |= match key.code {
            keymap::MAC_COMMAND | keymap::MAC_RIGHT_COMMAND => CGEventFlags::CGEventFlagCommand,
            keymap::MAC_SHIFT | keymap::MAC_RIGHT_SHIFT => CGEventFlags::CGEventFlagShift,
            keymap::MAC_OPTION | keymap::MAC_RIGHT_OPTION => CGEventFlags::CGEventFlagAlternate,
            keymap::MAC_CONTROL | keymap::MAC_RIGHT_CONTROL => CGEventFlags::CGEventFlagControl,
            _ => CGEventFlags::CGEventFlagNull,
        };
    }
    flags
}

fn post(event: CGEvent, modifiers: &[MacKey]) {
    event.set_flags(flags(modifiers));
    event.post(CGEventTapLocation::HID);
}

fn cg_button(button: MouseButton) -> CGMouseButton {
    match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        _ => CGMouseButton::Center,
    }
}

/// macOS numbers the extra buttons from 2 (middle).
fn button_number(button: MouseButton) -> i64 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

pub fn mouse_move(x: u16, y: u16, monitor: &MonitorInfo) {
    let point = CGPoint {
        x: monitor.x as f64 + x as f64 / 65535.0 * (monitor.width as f64 - 1.0).max(0.0),
        y: monitor.y as f64 + y as f64 / 65535.0 * (monitor.height as f64 - 1.0).max(0.0),
    };
    let mut state = STATE.lock().unwrap();
    state.position = point;
    let held = state.buttons.first().copied();
    let event_type = match held {
        Some(MouseButton::Left) => CGEventType::LeftMouseDragged,
        Some(MouseButton::Right) => CGEventType::RightMouseDragged,
        Some(_) => CGEventType::OtherMouseDragged,
        None => CGEventType::MouseMoved,
    };
    let button = held.map_or(CGMouseButton::Left, cg_button);
    let Some(source) = source() else { return };
    if let Ok(event) = CGEvent::new_mouse_event(source, event_type, point, button) {
        if let Some(b) = held {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button_number(b));
        }
        post(event, &state.modifiers);
    }
}

pub fn mouse_button(button: MouseButton, down: bool) {
    let mut state = STATE.lock().unwrap();
    let position = state.position;

    if down {
        let repeat = state.last_click.is_some_and(|(b, at, p)| {
            b == button
                && at.elapsed() < DOUBLE_CLICK_TIME
                && (p.x - position.x).abs() <= DOUBLE_CLICK_DISTANCE
                && (p.y - position.y).abs() <= DOUBLE_CLICK_DISTANCE
        });
        state.click_count = if repeat { state.click_count + 1 } else { 1 };
        state.last_click = Some((button, Instant::now(), position));
        if !state.buttons.contains(&button) {
            state.buttons.push(button);
        }
    } else {
        state.buttons.retain(|b| *b != button);
    }

    let event_type = match (button, down) {
        (MouseButton::Left, true) => CGEventType::LeftMouseDown,
        (MouseButton::Left, false) => CGEventType::LeftMouseUp,
        (MouseButton::Right, true) => CGEventType::RightMouseDown,
        (MouseButton::Right, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    };
    let Some(source) = source() else { return };
    if let Ok(event) = CGEvent::new_mouse_event(source, event_type, position, cg_button(button)) {
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button_number(button));
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, state.click_count);
        post(event, &state.modifiers);
    }
}

/// Windows sends 120 per wheel notch, which is three lines by default.
pub fn wheel(horizontal: bool, delta: i16) {
    let lines = (delta as i32 * 3 / 120).clamp(-100, 100);
    let lines = if lines == 0 { delta.signum() as i32 } else { lines };
    // A positive Windows horizontal delta means scroll right, which is negative here.
    let (vertical, sideways) = if horizontal { (0, -lines) } else { (lines, 0) };
    let state = STATE.lock().unwrap();
    let Some(source) = source() else { return };
    if let Ok(event) = CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, vertical, sideways, 0) {
        post(event, &state.modifiers);
    }
}

pub fn key(scan: u16, extended: bool, down: bool) {
    let Some(key) = keymap::to_mac(scan, extended) else { return };
    let mut state = STATE.lock().unwrap();
    if key.is_modifier() {
        if down {
            if !state.modifiers.contains(&key) {
                state.modifiers.push(key);
            }
        } else {
            state.modifiers.retain(|k| *k != key);
        }
    }
    let Some(source) = source() else { return };
    if let Ok(event) = CGEvent::new_keyboard_event(source, key.code, down) {
        post(event, &state.modifiers);
    }
}
