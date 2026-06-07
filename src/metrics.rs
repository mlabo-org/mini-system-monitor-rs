use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use sysinfo::{Components, System};

const BYTES_PER_GIB: f32 = 1024.0 * 1024.0 * 1024.0;
const TEMP_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_millis(450);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub memory_used_gib: f32,
    pub memory_total_gib: f32,
    pub cpu_temp_c: Option<f32>,
    pub temp_source: Option<String>,
}

pub struct MetricsSampler {
    system: System,
    components: Components,
    last_temp: Option<(f32, String)>,
    last_temp_check: Option<Instant>,
}

impl MetricsSampler {
    pub fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_cpu_usage();

        Self {
            system,
            components: Components::new_with_refreshed_list(),
            last_temp: None,
            last_temp_check: None,
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();

        let total_memory = self.system.total_memory();
        let used_memory = self.system.used_memory().min(total_memory);
        let memory_percent = percent(used_memory, total_memory);

        let temp = self.cpu_temperature();

        Snapshot {
            cpu_percent: self.system.global_cpu_usage().clamp(0.0, 100.0),
            memory_percent,
            memory_used_gib: used_memory as f32 / BYTES_PER_GIB,
            memory_total_gib: total_memory as f32 / BYTES_PER_GIB,
            cpu_temp_c: temp.as_ref().map(|(value, _)| *value),
            temp_source: temp.map(|(_, source)| source),
        }
    }

    fn cpu_temperature(&mut self) -> Option<(f32, String)> {
        let now = Instant::now();
        if self
            .last_temp_check
            .is_some_and(|last| now.duration_since(last) < TEMP_REFRESH_INTERVAL)
        {
            return self.last_temp.clone();
        }

        self.last_temp_check = Some(now);
        self.components.refresh(false);

        self.last_temp = temperature_from_platform()
            .or_else(|| temperature_from_components(&self.components))
            .or_else(temperature_from_external_commands);
        self.last_temp.clone()
    }
}

impl Default for MetricsSampler {
    fn default() -> Self {
        Self::new()
    }
}

fn percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        ((used as f32 / total as f32) * 100.0).clamp(0.0, 100.0)
    }
}

fn temperature_from_components(components: &Components) -> Option<(f32, String)> {
    components
        .iter()
        .filter_map(|component| {
            let value = component.temperature()?;
            if value.is_finite() && plausible_cpu_temp(value) {
                Some((value, format!("sysinfo:{}", component.label())))
            } else {
                None
            }
        })
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
}

fn temperature_from_platform() -> Option<(f32, String)> {
    #[cfg(target_os = "macos")]
    {
        macos_iohid::temperature_from_iohid()
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn temperature_from_external_commands() -> Option<(f32, String)> {
    let commands: &[(&str, &[&str], &str)] = &[
        ("osx-cpu-temp", &[], "osx-cpu-temp"),
        ("istats", &["cpu", "temp"], "istats cpu temp"),
        (
            "powermetrics",
            &["--samplers", "smc", "-n", "1", "-i", "1"],
            "powermetrics smc",
        ),
    ];

    commands.iter().find_map(|(program, args, source)| {
        let output = run_command_with_timeout(program, *args, COMMAND_TIMEOUT)?;
        parse_cpu_temperature_c(&output).map(|value| (value, (*source).to_owned()))
    })
}

fn run_command_with_timeout<S, I>(program: &str, args: I, timeout: Duration) -> Option<String>
where
    S: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
{
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                return Some(text);
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

fn parse_cpu_temperature_c(text: &str) -> Option<f32> {
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("cpu") || lower.contains("die") || lower.contains("thermal")
        })
        .find_map(parse_first_plausible_temp)
        .or_else(|| parse_first_plausible_temp(text))
}

fn parse_first_plausible_temp(text: &str) -> Option<f32> {
    text.split(|ch: char| {
        !(ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == '+' || ch == ',')
    })
    .filter_map(|part| {
        let normalized = part.trim().replace(',', ".");
        normalized.parse::<f32>().ok()
    })
    .find(|value| plausible_cpu_temp(*value))
}

fn plausible_cpu_temp(value: f32) -> bool {
    (0.0..=125.0).contains(&value)
}

fn select_iohid_temperature<I>(candidates: I) -> Option<(f32, String)>
where
    I: IntoIterator<Item = (f32, String)>,
{
    candidates
        .into_iter()
        .filter_map(|(value, product)| {
            if value.is_finite() && plausible_cpu_temp(value) {
                iohid_sensor_priority(&product).map(|priority| (priority, value, product))
            } else {
                None
            }
        })
        .max_by(
            |(left_priority, left_value, _), (right_priority, right_value, _)| {
                left_priority
                    .cmp(right_priority)
                    .then_with(|| left_value.total_cmp(right_value))
            },
        )
        .map(|(_, value, product)| (value, iohid_temperature_source(&product)))
}

fn iohid_sensor_priority(product: &str) -> Option<u8> {
    let lower = product.to_ascii_lowercase();
    if !lower.contains("pmu") {
        return None;
    }

    if lower.contains("tdie") || lower.contains(" die") {
        return Some(2);
    }

    let has_tp_sensor = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part.starts_with("tp"));
    has_tp_sensor.then_some(1)
}

fn iohid_temperature_source(product: &str) -> String {
    match iohid_sensor_priority(product) {
        Some(2) => "IOHID PMU tdie".to_owned(),
        Some(1) => "IOHID PMU TP".to_owned(),
        _ => "Apple Silicon SoC".to_owned(),
    }
}

#[cfg(target_os = "macos")]
mod macos_iohid {
    use std::ffi::{CStr, CString, c_char, c_void};

    use super::select_iohid_temperature;

    type CFIndex = isize;
    type Boolean = u8;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const IOHID_EVENT_TYPE_TEMPERATURE: i64 = 15;
    const IOHID_EVENT_FIELD_TEMPERATURE: u32 = (IOHID_EVENT_TYPE_TEMPERATURE as u32) << 16;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(the_array: *const c_void) -> CFIndex;
        fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: CFIndex) -> *const c_void;
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFRelease(cf: *const c_void);
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFStringGetCString(
            the_string: *const c_void,
            buffer: *mut c_char,
            buffer_size: CFIndex,
            encoding: u32,
        ) -> Boolean;
        fn CFStringGetLength(the_string: *const c_void) -> CFIndex;
        fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
        fn CFStringGetTypeID() -> usize;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDEventGetFloatValue(event: *const c_void, field: u32) -> f64;
        fn IOHIDEventSystemClientCopyServices(client: *const c_void) -> *const c_void;
        fn IOHIDEventSystemClientCreate(allocator: *const c_void) -> *const c_void;
        fn IOHIDServiceClientCopyEvent(
            service: *const c_void,
            event_type: i64,
            options: i32,
            timeout: i64,
        ) -> *const c_void;
        fn IOHIDServiceClientCopyProperty(
            service: *const c_void,
            key: *const c_void,
        ) -> *const c_void;
    }

    struct OwnedCfRef(*const c_void);

    impl OwnedCfRef {
        fn new(value: *const c_void) -> Option<Self> {
            (!value.is_null()).then_some(Self(value))
        }

        fn as_ptr(&self) -> *const c_void {
            self.0
        }
    }

    impl Drop for OwnedCfRef {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CFRelease(self.0);
                }
            }
        }
    }

    pub(super) fn temperature_from_iohid() -> Option<(f32, String)> {
        unsafe {
            let client = OwnedCfRef::new(IOHIDEventSystemClientCreate(std::ptr::null()))?;
            let services = OwnedCfRef::new(IOHIDEventSystemClientCopyServices(client.as_ptr()))?;
            let product_key = cf_string("Product")?;

            let count = CFArrayGetCount(services.as_ptr());
            if count <= 0 {
                return None;
            }

            let candidates = (0..count).filter_map(|index| {
                let service = CFArrayGetValueAtIndex(services.as_ptr(), index);
                if service.is_null() {
                    return None;
                }

                let product = copy_string_property(service, product_key.as_ptr())?;
                let event = OwnedCfRef::new(IOHIDServiceClientCopyEvent(
                    service,
                    IOHID_EVENT_TYPE_TEMPERATURE,
                    0,
                    0,
                ))?;
                let value = IOHIDEventGetFloatValue(event.as_ptr(), IOHID_EVENT_FIELD_TEMPERATURE);
                Some((value as f32, product))
            });

            select_iohid_temperature(candidates)
        }
    }

    fn cf_string(value: &str) -> Option<OwnedCfRef> {
        let c_value = CString::new(value).ok()?;
        unsafe {
            OwnedCfRef::new(CFStringCreateWithCString(
                std::ptr::null(),
                c_value.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            ))
        }
    }

    unsafe fn copy_string_property(service: *const c_void, key: *const c_void) -> Option<String> {
        let property = OwnedCfRef::new(unsafe { IOHIDServiceClientCopyProperty(service, key) })?;
        unsafe { cf_string_to_string(property.as_ptr()) }
    }

    unsafe fn cf_string_to_string(value: *const c_void) -> Option<String> {
        if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            return None;
        }

        let length = unsafe { CFStringGetLength(value) };
        let max_size =
            unsafe { CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8) } + 1;
        if max_size <= 1 {
            return None;
        }

        let mut buffer = vec![0_i8; max_size as usize];
        let ok = unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                max_size,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if ok == 0 {
            return None;
        }

        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osx_cpu_temp_output() {
        assert_eq!(parse_cpu_temperature_c("61.2°C"), Some(61.2));
    }

    #[test]
    fn parses_istats_cpu_output() {
        let text = "CPU temp: 48.88°C\nFan: 1200 rpm";
        assert_eq!(parse_cpu_temperature_c(text), Some(48.88));
    }

    #[test]
    fn parses_powermetrics_smc_output() {
        let text = "CPU die temperature: 57.4 C\nGPU die temperature: 49.1 C";
        assert_eq!(parse_cpu_temperature_c(text), Some(57.4));
    }

    #[test]
    fn rejects_implausible_temperatures() {
        assert_eq!(parse_cpu_temperature_c("CPU die temperature: 999 C"), None);
    }

    #[test]
    fn selects_hottest_iohid_tdie_sensor() {
        let candidates = vec![
            (49.0, "PMU TP0d".to_owned()),
            (52.5, "PMU tdie1".to_owned()),
            (51.0, "PMU tdie2".to_owned()),
            (80.0, "Battery".to_owned()),
        ];

        assert_eq!(
            select_iohid_temperature(candidates),
            Some((52.5, "IOHID PMU tdie".to_owned()))
        );
    }

    #[test]
    fn falls_back_to_iohid_tp_sensor_when_tdie_is_absent() {
        let candidates = vec![(47.2, "PMU TP0d".to_owned()), (48.3, "PMU TP1w".to_owned())];

        assert_eq!(
            select_iohid_temperature(candidates),
            Some((48.3, "IOHID PMU TP".to_owned()))
        );
    }

    #[test]
    fn rejects_unrelated_iohid_temperatures() {
        let candidates = vec![
            (52.0, "Battery".to_owned()),
            (999.0, "PMU tdie1".to_owned()),
        ];

        assert_eq!(select_iohid_temperature(candidates), None);
    }
}
