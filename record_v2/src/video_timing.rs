use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Mutex,
    time::Instant,
};

use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{
    fmt::{
        FmtContext,
        format::{FormatEvent, FormatFields, Writer},
    },
    registry::LookupSpan,
};

const MAX_PENDING_FRAMES: usize = 512;
const HEALTHY_REPORT_SAMPLE_INTERVAL: u64 = 30;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

#[derive(Clone, Debug)]
enum FieldValue {
    U64(u64),
    I64(i64),
    Bool(bool),
    Text(String),
}

#[derive(Clone, Debug, Default)]
struct TimingFields {
    values: Vec<(&'static str, FieldValue)>,
}

impl TimingFields {
    fn insert(&mut self, name: &'static str, value: FieldValue) {
        if let Some((_, current)) = self.values.iter_mut().find(|(field, _)| *field == name) {
            *current = value;
        } else {
            self.values.push((name, value));
        }
    }

    fn u64(&self, name: &str) -> Option<u64> {
        self.values
            .iter()
            .find_map(|(field, value)| match (*field == name, value) {
                (true, FieldValue::U64(value)) => Some(*value),
                (true, FieldValue::I64(value)) => u64::try_from(*value).ok(),
                _ => None,
            })
    }

    fn bool(&self, name: &str) -> Option<bool> {
        self.values
            .iter()
            .find_map(|(field, value)| match (*field == name, value) {
                (true, FieldValue::Bool(value)) => Some(*value),
                _ => None,
            })
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find_map(|(field, value)| match (*field == name, value) {
                (true, FieldValue::Text(value)) => Some(value.as_str()),
                _ => None,
            })
    }

    fn optional_debug_text(&self, name: &str) -> Option<&str> {
        let value = self.text(name)?;
        if value == "None" {
            return None;
        }
        Some(
            value
                .strip_prefix("Some(")
                .and_then(|value| value.strip_suffix(')'))
                .unwrap_or(value),
        )
    }

    fn stage(&self) -> &str {
        self.text("stage").unwrap_or("video.event")
    }
}

impl Visit for TimingFields {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.insert(field.name(), FieldValue::U64(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field.name(), FieldValue::I64(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field.name(), FieldValue::Bool(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.insert(field.name(), FieldValue::Text(value.to_string()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.insert(field.name(), FieldValue::Text(value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.insert(field.name(), FieldValue::Text(format!("{value:?}")));
    }
}

#[derive(Clone, Default)]
struct FrameReport {
    frame_id: u64,
    stages: Vec<TimingFields>,
}

impl FrameReport {
    fn record(&mut self, mut fields: TimingFields) {
        let is_av1 = fields.stage() == "av1.encode";
        let stage = fields.stage();
        if let Some(current) = self
            .stages
            .iter_mut()
            .find(|current| current.stage() == stage)
        {
            // A delayed AV1 packet is correlated back to its emitted input and
            // replaces that input's earlier NeedMoreData event. Preserve peak
            // buffering and rebuild evidence before replacing stage details.
            if is_av1 {
                let buffered_streak = current
                    .u64("need_more_data_streak")
                    .unwrap_or_default()
                    .max(fields.u64("need_more_data_streak").unwrap_or_default());
                fields.insert("need_more_data_streak", FieldValue::U64(buffered_streak));
                if current.bool("context_rebuilt") == Some(true) {
                    fields.insert("context_rebuilt", FieldValue::Bool(true));
                    let rebuild_us = current
                        .u64("context_rebuild_us")
                        .unwrap_or_default()
                        .max(fields.u64("context_rebuild_us").unwrap_or_default());
                    fields.insert("context_rebuild_us", FieldValue::U64(rebuild_us));
                }
            }
            *current = fields;
        } else {
            self.stages.push(fields);
        }
    }

    fn field(&self, name: &str) -> Option<u64> {
        self.stages.iter().rev().find_map(|stage| stage.u64(name))
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.stages.iter().rev().find_map(|stage| stage.text(name))
    }

    fn bool(&self, name: &str) -> Option<bool> {
        self.stages.iter().rev().find_map(|stage| stage.bool(name))
    }

    fn error(&self) -> Option<&str> {
        self.stages
            .iter()
            .rev()
            .find_map(|stage| stage.optional_debug_text("error"))
    }

    fn stage(&self, name: &str) -> Option<&TimingFields> {
        self.stages.iter().find(|stage| stage.stage() == name)
    }

    fn final_stage(&self) -> Option<&TimingFields> {
        self.stage("outgoing.complete")
    }
}

#[derive(Default)]
struct ReportState {
    frames: HashMap<u64, FrameReport>,
    insertion_order: VecDeque<u64>,
    stale_replacements: u64,
    healthy_reports: u64,
}

impl ReportState {
    fn record(&mut self, frame_id: u64, fields: TimingFields) {
        if !self.frames.contains_key(&frame_id) {
            self.insertion_order.push_back(frame_id);
        }
        let report = self.frames.entry(frame_id).or_insert_with(|| FrameReport {
            frame_id,
            stages: Vec::with_capacity(12),
        });
        report.record(fields);
        self.evict_old_frames();
    }

    fn evict_old_frames(&mut self) {
        while self.frames.len() > MAX_PENDING_FRAMES {
            let Some(frame_id) = self.insertion_order.pop_front() else {
                break;
            };
            self.frames.remove(&frame_id);
        }
        while self
            .insertion_order
            .front()
            .is_some_and(|frame_id| !self.frames.contains_key(frame_id))
        {
            self.insertion_order.pop_front();
        }
    }

    fn should_render(&mut self, report: &FrameReport, stale_replacements: u64) -> bool {
        if report_is_anomalous(report, stale_replacements) {
            return true;
        }

        self.healthy_reports = self.healthy_reports.saturating_add(1);
        self.healthy_reports == 1
            || self
                .healthy_reports
                .is_multiple_of(HEALTHY_REPORT_SAMPLE_INTERVAL)
    }
}

/// Human-focused formatter for structured `video_timing` events.
///
/// Events remain structured at call sites. Formatter aggregates stages by
/// frame and prints one report only after Discord finishes sending frame.
pub(crate) struct VideoTimingFormatter {
    started_at: Instant,
    state: Mutex<ReportState>,
}

impl VideoTimingFormatter {
    pub(crate) fn new() -> Self {
        Self {
            started_at: Instant::now(),
            state: Mutex::new(ReportState::default()),
        }
    }
}

impl<S, N> FormatEvent<S, N> for VideoTimingFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = TimingFields::default();
        event.record(&mut fields);

        let Some(frame_id) = correlated_frame_id(&fields) else {
            return render_single_stage(
                &fields,
                self.started_at.elapsed().as_secs_f64(),
                writer.has_ansi_escapes(),
                &mut writer,
            );
        };

        let is_stale_replacement =
            fields.stage() == "channel.publish" && fields.bool("stale_replaced") == Some(true);
        let is_complete = fields.stage() == "outgoing.complete"
            && (fields.bool("emitted") == Some(true) || fields.bool("failed") == Some(true));

        let (report, stale_replacements, should_render) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if is_stale_replacement {
                state.stale_replacements = state.stale_replacements.saturating_add(1);
            }
            state.record(frame_id, fields);
            if is_complete {
                let report = state.frames.remove(&frame_id);
                let stale_replacements = std::mem::take(&mut state.stale_replacements);
                let should_render = report
                    .as_ref()
                    .is_some_and(|report| state.should_render(report, stale_replacements));
                (report, stale_replacements, should_render)
            } else {
                return Ok(());
            }
        };

        if should_render && let Some(mut report) = report {
            report.stages.sort_by_key(|stage| stage_rank(stage.stage()));
            render_report(
                &report,
                stale_replacements,
                self.started_at.elapsed().as_secs_f64(),
                writer.has_ansi_escapes(),
                &mut writer,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Component {
    field: &'static str,
    label: &'static str,
    actionable: bool,
}

const SCREEN_COMPONENTS: &[Component] = &[
    Component {
        field: "wait_us",
        label: "frame wait (expected cadence)",
        actionable: false,
    },
    Component {
        field: "normalize_us",
        label: "normalize + downscale",
        actionable: true,
    },
];
const WEBCAM_COMPONENTS: &[Component] = &[
    Component {
        field: "wait_us",
        label: "device frame wait",
        actionable: false,
    },
    Component {
        field: "decode_us",
        label: "camera format decode",
        actionable: true,
    },
];
const CAPTURE_COMPONENTS: &[Component] = &[
    Component {
        field: "capture_us",
        label: "capture + normalize envelope",
        actionable: false,
    },
    Component {
        field: "publish_us",
        label: "publish to latest-frame queue",
        actionable: true,
    },
];
const SCALE_COMPONENTS: &[Component] = &[Component {
    field: "elapsed_us",
    label: "software pixel scaling",
    actionable: true,
}];
const CHANNEL_COMPONENTS: &[Component] = &[Component {
    field: "lock_wait_us",
    label: "channel lock wait",
    actionable: true,
}];
const INPUT_COMPONENTS: &[Component] = &[
    Component {
        field: "pacing_wait_us",
        label: "frame pacing wait",
        actionable: false,
    },
    Component {
        field: "input_wait_us",
        label: "input queue wait",
        actionable: false,
    },
];
const H264_COMPONENTS: &[Component] = &[
    Component {
        field: "rgb_to_yuv_us",
        label: "RGB → YUV conversion",
        actionable: true,
    },
    Component {
        field: "codec_us",
        label: "OpenH264 encode core",
        actionable: true,
    },
    Component {
        field: "output_copy_us",
        label: "encoded output copy",
        actionable: true,
    },
];
const AV1_COMPONENTS: &[Component] = &[
    Component {
        field: "state_setup_us",
        label: "encoder state setup",
        actionable: true,
    },
    Component {
        field: "frame_alloc_us",
        label: "rav1e frame allocation",
        actionable: true,
    },
    Component {
        field: "rgb_to_yuv_us",
        label: "RGB → YUV conversion",
        actionable: true,
    },
    Component {
        field: "send_frame_us",
        label: "rav1e frame submission",
        actionable: true,
    },
    Component {
        field: "receive_packet_us",
        label: "rav1e packet production",
        actionable: true,
    },
];
const H264_DECODE_COMPONENTS: &[Component] = &[
    Component {
        field: "codec_us",
        label: "OpenH264 decode",
        actionable: true,
    },
    Component {
        field: "rgba_alloc_us",
        label: "RGBA allocation + zero-fill",
        actionable: true,
    },
    Component {
        field: "yuv_to_rgba_us",
        label: "YUV → RGBA conversion",
        actionable: true,
    },
];
const INCOMING_COMPONENTS: &[Component] = &[
    Component {
        field: "receive_bookkeeping_us",
        label: "receive bookkeeping/NACK",
        actionable: true,
    },
    Component {
        field: "dave_lock_wait_us",
        label: "DAVE lock wait",
        actionable: true,
    },
    Component {
        field: "dave_decrypt_preflight_us",
        label: "DAVE decrypt allocation/preflight",
        actionable: true,
    },
    Component {
        field: "dave_decrypt_transform_us",
        label: "DAVE decrypt transform",
        actionable: true,
    },
    Component {
        field: "decode_us",
        label: "decode envelope",
        actionable: true,
    },
];
const PREVIEW_COMPONENTS: &[Component] = &[Component {
    field: "scale_us",
    label: "preview frame scaling",
    actionable: true,
}];
const WORKER_COMPONENTS: &[Component] = &[
    Component {
        field: "worker_queue_us",
        label: "blocking-worker queue",
        actionable: true,
    },
    Component {
        field: "elapsed_us",
        label: "worker execution envelope",
        actionable: false,
    },
];
const TRANSPORT_COMPONENTS: &[Component] = &[
    Component {
        field: "dave_lock_wait_us",
        label: "DAVE lock wait",
        actionable: true,
    },
    Component {
        field: "dave_preflight_us",
        label: "DAVE allocation + preflight",
        actionable: true,
    },
    Component {
        field: "dave_transform_us",
        label: "DAVE transform + AES-GCM",
        actionable: true,
    },
    Component {
        field: "packetize_us",
        label: "codec packetization",
        actionable: true,
    },
    Component {
        field: "history_us",
        label: "RTX history copy",
        actionable: true,
    },
    Component {
        field: "aead_us",
        label: "RTP AEAD encryption",
        actionable: true,
    },
    Component {
        field: "udp_send_us",
        label: "UDP send syscalls",
        actionable: true,
    },
    Component {
        field: "pacing_us",
        label: "packet pacing wait",
        actionable: false,
    },
    Component {
        field: "sender_report_us",
        label: "RTCP sender report",
        actionable: true,
    },
];
const COMPLETE_COMPONENTS: &[Component] = &[
    Component {
        field: "signaling_us",
        label: "Discord signaling",
        actionable: true,
    },
    Component {
        field: "announce_settle_us",
        label: "first-frame announce settle",
        actionable: false,
    },
    Component {
        field: "job_us",
        label: "encode + transport job envelope",
        actionable: false,
    },
];
const NO_COMPONENTS: &[Component] = &[];

fn components(stage: &str) -> &'static [Component] {
    match stage {
        "screen.capture" => SCREEN_COMPONENTS,
        "webcam.capture" => WEBCAM_COMPONENTS,
        "capture.bounded" => CAPTURE_COMPONENTS,
        "frame.scale" => SCALE_COMPONENTS,
        "channel.publish" | "channel.consume" | "channel.try_consume" => CHANNEL_COMPONENTS,
        "outgoing.input_wait" => INPUT_COMPONENTS,
        "h264.encode" => H264_COMPONENTS,
        "av1.encode" => AV1_COMPONENTS,
        "h264.decode" => H264_DECODE_COMPONENTS,
        "incoming.receive" => INCOMING_COMPONENTS,
        "ui.preview_select" => PREVIEW_COMPONENTS,
        "outgoing.encode_worker" => WORKER_COMPONENTS,
        "outgoing.transport" => TRANSPORT_COMPONENTS,
        "outgoing.complete" => COMPLETE_COMPONENTS,
        _ => NO_COMPONENTS,
    }
}

fn stage_rank(stage: &str) -> u8 {
    match stage {
        "webcam.capture" | "screen.capture" => 10,
        "frame.scale" => 11,
        "capture.bounded" => 12,
        "channel.publish" => 20,
        "channel.consume" | "channel.try_consume" => 21,
        "outgoing.input_wait" => 30,
        "outgoing.accept" => 31,
        "h264.encode" | "av1.encode" => 40,
        "outgoing.encode_worker" => 41,
        "outgoing.transport" => 50,
        "outgoing.complete" => 60,
        _ => 55,
    }
}

fn stage_group(stage: &str) -> &'static str {
    match stage_rank(stage) {
        0..=19 => "CAPTURE",
        20..=29 => "LATEST-FRAME QUEUE",
        30..=39 => "DISCORD SCHEDULING",
        40..=49 => "ENCODE",
        50..=59 => "TRANSPORT",
        _ => "FINAL",
    }
}

fn stage_total_us(fields: &TimingFields) -> Option<u64> {
    match fields.stage() {
        "outgoing.complete" => fields.u64("pipeline_elapsed_us"),
        "ui.preview_select" => fields.u64("scale_us"),
        "outgoing.encode_worker" => fields
            .u64("worker_queue_us")
            .zip(fields.u64("elapsed_us"))
            .map(|(queue, work)| queue.saturating_add(work)),
        _ => fields.u64("elapsed_us").or_else(|| fields.u64("total_us")),
    }
}

fn stage_label(stage: &str) -> &str {
    match stage {
        "screen.capture" => "screen capture + normalize",
        "webcam.capture" => "webcam capture + decode",
        "capture.bounded" => "capture envelope → queue",
        "frame.scale" => "software frame scale",
        "channel.publish" => "publish newest frame",
        "channel.consume" => "consume newest frame",
        "channel.try_consume" => "consume preview frame",
        "outgoing.input_wait" => "Discord frame pacing/input",
        "outgoing.accept" => "Discord accept frame",
        "h264.encode" => "H264 encode",
        "av1.encode" => "AV1 encode",
        "h264.decode" => "H264 decode",
        "incoming.receive" => "incoming Discord frame",
        "ui.preview_select" => "UI preview scale",
        "ui.texture_handle" => "UI texture upload",
        "capture.open" => "capture device open",
        "outgoing.encode_worker" => "encode worker envelope",
        "outgoing.transport" => "encrypt + packetize + UDP",
        "outgoing.complete" => "capture → Discord send",
        other => other,
    }
}

fn correlated_frame_id(fields: &TimingFields) -> Option<u64> {
    if !matches!(
        fields.stage(),
        "screen.capture"
            | "webcam.capture"
            | "capture.bounded"
            | "frame.scale"
            | "channel.publish"
            | "channel.consume"
            | "channel.try_consume"
            | "outgoing.input_wait"
            | "outgoing.accept"
            | "h264.encode"
            | "av1.encode"
            | "outgoing.encode_worker"
            | "outgoing.transport"
            | "outgoing.complete"
    ) {
        return None;
    }

    fields
        .u64("emitted_frame_id")
        .filter(|frame_id| *frame_id != 0)
        .or_else(|| fields.u64("frame_id").filter(|frame_id| *frame_id != 0))
}

fn report_is_anomalous(report: &FrameReport, stale_replacements: u64) -> bool {
    let final_stage = report.final_stage();
    let budget_us = final_stage
        .and_then(|stage| stage.u64("frame_budget_us"))
        .unwrap_or_default();
    let discord_us = final_stage
        .and_then(|stage| stage.u64("discord_us"))
        .unwrap_or_default();
    let over_budget = final_stage
        .and_then(|stage| stage.bool("over_budget"))
        .unwrap_or(budget_us > 0 && discord_us > budget_us);
    let output_gap_us = report.field("output_gap_us").unwrap_or_default();

    stale_replacements > 0
        || final_stage.and_then(|stage| stage.bool("failed")) == Some(true)
        || report.error().is_some()
        || over_budget
        || (budget_us > 0 && output_gap_us > budget_us.saturating_mul(2))
        || report.bool("keyframe") == Some(true)
        || report.bool("context_rebuilt") == Some(true)
        || report.bool("emitted_context_rebuilt") == Some(true)
        || report.field("need_more_data_streak").unwrap_or_default() > 0
}

fn hottest_component(report: &FrameReport) -> Option<(&'static str, u64)> {
    report
        .stages
        .iter()
        .flat_map(|stage| {
            components(stage.stage())
                .iter()
                .filter(|component| component.actionable)
                .filter_map(|component| {
                    stage
                        .u64(component.field)
                        .map(|duration| (component.label, duration))
                })
        })
        .max_by_key(|(_, duration)| *duration)
}

fn render_report(
    report: &FrameReport,
    stale_replacements: u64,
    uptime_seconds: f64,
    ansi: bool,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let final_stage = report.final_stage();
    let pipeline_us = final_stage
        .and_then(|stage| stage.u64("pipeline_elapsed_us"))
        .or_else(|| report.field("pipeline_elapsed_us"))
        .unwrap_or_default();
    let budget_us = final_stage
        .and_then(|stage| stage.u64("frame_budget_us"))
        .unwrap_or_default();
    let frame_work_us = final_stage
        .and_then(|stage| stage.u64("discord_us"))
        .unwrap_or(pipeline_us);
    let kind = final_stage
        .and_then(|stage| stage.text("kind"))
        .or_else(|| report.text("kind"))
        .unwrap_or("Video");
    let ratio = (budget_us > 0).then(|| frame_work_us as f64 / budget_us as f64);
    let over_budget = final_stage
        .and_then(|stage| stage.bool("over_budget"))
        .unwrap_or_else(|| ratio.is_some_and(|ratio| ratio > 1.0));

    let style = Styles::new(ansi);
    writeln!(output)?;
    writeln!(
        output,
        "{}╭─ {}{}VIDEO PIPELINE{} · {}{}{} · frame #{} {}+{uptime_seconds:.3}s{}",
        style.dim,
        style.bold,
        style.cyan,
        style.reset,
        style.bold,
        kind.to_uppercase(),
        style.reset,
        report.frame_id,
        style.dim,
        style.reset,
    )?;
    write!(
        output,
        "{}│{}  {}CAPTURE CALL → SEND{:>14.3} ms{}",
        style.dim,
        style.reset,
        style.bold,
        micros_to_ms(pipeline_us),
        style.reset,
    )?;
    writeln!(output)?;
    write!(
        output,
        "{}│{}  {}DISCORD FRAME WORK{:>15.3} ms{}",
        style.dim,
        style.reset,
        style.bold,
        micros_to_ms(frame_work_us),
        style.reset,
    )?;
    if let Some(ratio) = ratio {
        let budget_color = if over_budget { style.red } else { style.green };
        write!(
            output,
            "   {budget_color}{ratio:.1}× {} BUDGET{} {}({:.1} FPS · {:.3} ms){}",
            if over_budget { "OVER" } else { "OF" },
            style.reset,
            style.dim,
            frames_per_second(budget_us),
            micros_to_ms(budget_us),
            style.reset,
        )?;
    }
    writeln!(output)?;

    render_output_diagnostics(report, budget_us, &style, output)?;

    if final_stage.and_then(|stage| stage.bool("failed")) == Some(true) {
        writeln!(
            output,
            "{}│{}  {}FAILED{}  {}",
            style.dim,
            style.reset,
            style.red,
            style.reset,
            report.error().unwrap_or("unknown video send error"),
        )?;
    }

    if let Some((label, duration_us)) = hottest_component(report) {
        writeln!(
            output,
            "{}│{}  {}HOTTEST ACTIONABLE STEP{}  {:<25} {}{:>9.3} ms{}",
            style.dim,
            style.reset,
            style.red,
            style.reset,
            label,
            style.red,
            micros_to_ms(duration_us),
            style.reset,
        )?;
    }

    if stale_replacements > 0 {
        writeln!(
            output,
            "{}│{}  {}DROPPED{}     {stale_replacements} stale frame{} replaced in latest-frame queue",
            style.dim,
            style.reset,
            style.yellow,
            style.reset,
            if stale_replacements == 1 { "" } else { "s" },
        )?;
    }

    let mut group = "";
    for stage in &report.stages {
        if stage_total_us(stage).is_none() {
            continue;
        }
        let next_group = stage_group(stage.stage());
        if group != next_group {
            group = next_group;
            writeln!(
                output,
                "{}├─ {}{}{}{}",
                style.dim, style.cyan, group, style.reset, style.dim
            )?;
        }
        render_stage(stage, pipeline_us, &style, output)?;
    }
    writeln!(
        output,
        "{}╰────────────────────────────────────────────────────────────────────{}",
        style.dim, style.reset
    )
}

fn render_output_diagnostics(
    report: &FrameReport,
    budget_us: u64,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    if let Some(output_gap_us) = report.field("output_gap_us") {
        if output_gap_us == 0 {
            writeln!(
                output,
                "{}│{}  {}WIRE OUTPUT GAP{}  n/a (first emitted frame)",
                style.dim, style.reset, style.bold, style.reset,
            )?;
        } else {
            let gap_color = if budget_us > 0 && output_gap_us > budget_us.saturating_mul(2) {
                style.red
            } else {
                style.green
            };
            writeln!(
                output,
                "{}│{}  {}WIRE OUTPUT GAP{}  {gap_color}{:>9.3} ms{} · observed {:.1} FPS",
                style.dim,
                style.reset,
                style.bold,
                style.reset,
                micros_to_ms(output_gap_us),
                style.reset,
                frames_per_second(output_gap_us),
            )?;
        }
    }

    let Some(av1) = report.stage("av1.encode") else {
        return Ok(());
    };
    let lag_frames = av1
        .u64("encoder_lag_frames")
        .or_else(|| av1.u64("lag_frames"))
        .unwrap_or_default();
    let submitted_force_keyframe = av1
        .bool("submitted_force_keyframe")
        .or_else(|| av1.bool("submitted_forced_keyframe"))
        .unwrap_or(false);
    let emitted_force_keyframe = av1
        .bool("emitted_force_keyframe")
        .or_else(|| av1.bool("emitted_forced_keyframe"))
        .unwrap_or(false);
    let keyframe = av1
        .bool("keyframe")
        .or_else(|| av1.bool("emitted_keyframe"))
        .unwrap_or(false);
    write!(
        output,
        "{}│{}  {}AV1 PROVENANCE{}  submit ",
        style.dim, style.reset, style.bold, style.reset,
    )?;
    match av1.u64("submitted_sequence") {
        Some(sequence) => write!(output, "#{sequence}")?,
        None => write!(output, "n/a")?,
    }
    write!(output, " → emit ")?;
    match av1.u64("emitted_sequence") {
        Some(sequence) => write!(output, "#{sequence}")?,
        None => write!(output, "n/a")?,
    }
    write!(
        output,
        " · lag {} frame{} · residence {}",
        lag_frames,
        if lag_frames == 1 { "" } else { "s" },
        duration_or_na(av1.u64("encoder_residence_us").unwrap_or_default()),
    )?;
    if keyframe {
        write!(output, " · {}KEYFRAME{}", style.yellow, style.reset)?;
    }
    writeln!(output)?;

    let context_generation = av1
        .u64("emitted_context_generation")
        .filter(|generation| *generation != 0)
        .or_else(|| av1.u64("context_generation"))
        .unwrap_or_default();
    let context_rebuilt = av1.bool("emitted_context_rebuilt").unwrap_or(false)
        || av1.bool("context_rebuilt").unwrap_or(false);
    let context_rebuild_us = av1
        .u64("emitted_context_rebuild_us")
        .filter(|duration| *duration != 0)
        .or_else(|| av1.u64("context_rebuild_us"))
        .unwrap_or_default();
    write!(
        output,
        "{}│{}  {}AV1 STATE{}       force current={} emitted={} · context #{context_generation}",
        style.dim,
        style.reset,
        style.bold,
        style.reset,
        submitted_force_keyframe,
        emitted_force_keyframe,
    )?;
    if context_rebuilt {
        write!(output, " rebuilt in {}", duration_or_na(context_rebuild_us),)?;
    } else {
        write!(output, " reused")?;
    }
    writeln!(
        output,
        " · buffered streak {} · encoded loop count {}",
        av1.u64("need_more_data_streak").unwrap_or_default(),
        av1.u64("encoded_loop_count").unwrap_or_default(),
    )
}

fn render_single_stage(
    fields: &TimingFields,
    uptime_seconds: f64,
    ansi: bool,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let style = Styles::new(ansi);
    if fields.stage() == "outgoing.control" {
        return render_video_control(fields, uptime_seconds, &style, output);
    }
    let duration = stage_total_us(fields).unwrap_or_default();
    writeln!(
        output,
        "{}VIDEO PERF{} · {:<30} {}{:>9.3} ms{}  {}+{uptime_seconds:.3}s{}",
        style.cyan,
        style.reset,
        stage_label(fields.stage()),
        style.bold,
        micros_to_ms(duration),
        style.reset,
        style.dim,
        style.reset,
    )?;
    render_component_rows(fields, duration, "              ", &style, output)?;
    render_single_stage_details(fields, &style, output)
}

fn render_video_control(
    fields: &TimingFields,
    uptime_seconds: f64,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    write!(
        output,
        "{}VIDEO CONTROL{} · {}",
        style.cyan,
        style.reset,
        fields.text("kind").unwrap_or("Video"),
    )?;
    if let Some(action) = fields.text("action") {
        write!(output, " · {action}")?;
    }
    if let Some(codec) = fields.text("codec") {
        write!(output, " · codec {codec}")?;
    }
    if let Some(reason) = fields.text("reason") {
        write!(output, " · reason {reason}")?;
    }
    if let Some(transition_id) = fields.u64("transition_id") {
        write!(output, " · transition #{transition_id}")?;
    }
    if let Some(protocol_version) = fields.u64("protocol_version") {
        write!(output, " · protocol v{protocol_version}")?;
    }
    if let Some(pending) = fields.u64("pending_transitions") {
        write!(output, " · pending {pending}")?;
    }
    if let Some(frame_id) = fields.u64("frame_id").filter(|frame_id| *frame_id != 0) {
        write!(output, " · frame #{frame_id}")?;
    } else if let Some(submitted_frame_id) = fields
        .u64("submitted_frame_id")
        .filter(|frame_id| *frame_id != 0)
    {
        write!(output, " · submit #{submitted_frame_id}")?;
        if let Some(emitted_frame_id) = fields
            .u64("emitted_frame_id")
            .filter(|frame_id| *frame_id != 0)
        {
            write!(output, " → emit #{emitted_frame_id}")?;
        }
    }
    if let Some(emitted) = fields.bool("emitted") {
        write!(output, " · emitted {emitted}")?;
    }
    if let Some(keyframe) = fields.bool("keyframe") {
        write!(output, " · keyframe {keyframe}")?;
    }
    if let Some(previous_quality) = fields.u64("previous_quality")
        && let Some(quality) = fields.u64("network_quality")
    {
        write!(output, " · quality {previous_quality}→{quality}")?;
    } else if let Some(quality) = fields.u64("network_quality") {
        write!(output, " · network quality {quality}")?;
    }
    if let Some(sink_quality) = fields.u64("sink_quality") {
        write!(output, " · sink quality {sink_quality}")?;
    }
    if let Some(fraction_lost) = fields.u64("fraction_lost") {
        write!(output, " · loss {fraction_lost}/256")?;
    }
    if let Some(requested) = fields.u64("requested") {
        write!(
            output,
            " · RTX {}/{}",
            fields.u64("sent").unwrap_or_default(),
            requested
        )?;
    }
    if let Some(elapsed_us) = fields.u64("elapsed_us") {
        write!(output, " · {:.3} ms", micros_to_ms(elapsed_us))?;
    }
    if fields.bool("failed") == Some(true) {
        write!(output, " · {}FAILED{}", style.red, style.reset)?;
    }
    if let Some(error) = fields.optional_debug_text("error") {
        write!(output, " · error {error}")?;
    }
    writeln!(
        output,
        "  {}+{uptime_seconds:.3}s{}",
        style.dim, style.reset,
    )
}

fn render_single_stage_details(
    fields: &TimingFields,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    match fields.stage() {
        "h264.decode" => {
            let decoder_thread_mode = fields
                .text("decoder_thread_mode")
                .unwrap_or("OpenH264 default");
            let decoder_id = fields.u64("decoder_id").unwrap_or_default();
            let frame_id = fields.u64("frame_id").unwrap_or_default();
            let access_unit_bytes = bytes(fields.u64("access_unit_bytes").unwrap_or_default());
            if fields.bool("failed") == Some(true) {
                writeln!(
                    output,
                    "{}              ╰─{} decoder #{decoder_id} · input #{frame_id} · {access_unit_bytes} access unit · {decoder_thread_mode} · failed: {}",
                    style.dim,
                    style.reset,
                    fields.text("error").unwrap_or("unknown decoder error"),
                )
            } else if fields.bool("emitted") == Some(true) {
                writeln!(
                    output,
                    "{}              ╰─{} decoder #{decoder_id} · input #{frame_id} · {access_unit_bytes} access unit → {}×{} RGBA ({}) · {} · {decoder_thread_mode}",
                    style.dim,
                    style.reset,
                    fields.u64("width").unwrap_or_default(),
                    fields.u64("height").unwrap_or_default(),
                    bytes(fields.u64("output_bytes").unwrap_or_default()),
                    fields
                        .text("conversion_path")
                        .unwrap_or("unknown conversion"),
                )
            } else {
                writeln!(
                    output,
                    "{}              ╰─{} decoder #{decoder_id} · input #{frame_id} · {access_unit_bytes} access unit · {decoder_thread_mode} · buffered/no picture",
                    style.dim, style.reset,
                )
            }
        }
        "incoming.receive" => {
            let source_interval_us = fields.u64("source_interval_us").unwrap_or_default();
            let ssrc = fields.u64("ssrc").unwrap_or_default();
            let decoder_id = fields.u64("decoder_id").unwrap_or_default();
            let stream_frame_index = fields.u64("stream_frame_index").unwrap_or_default();
            if source_interval_us == 0 {
                writeln!(
                    output,
                    "{}              ├─{} SSRC {ssrc} · decoder #{decoder_id} · stream frame #{stream_frame_index} · observed RTP AU cadence n/a (first sample)",
                    style.dim, style.reset,
                )?;
            } else {
                writeln!(
                    output,
                    "{}              ├─{} SSRC {ssrc} · decoder #{decoder_id} · stream frame #{stream_frame_index} · RTP Δ {} ticks · observed RTP AU cadence {:.1} FPS",
                    style.dim,
                    style.reset,
                    fields.u64("rtp_delta_ticks").unwrap_or_default(),
                    frames_per_second(source_interval_us),
                )?;
            }
            let status = if fields.bool("failed") == Some(true) {
                "failed"
            } else if fields.bool("emitted") == Some(true) {
                "emitted"
            } else {
                "buffered"
            };
            write!(
                output,
                "{}              ╰─{} AU gap {} · output gap {} · packet span {:>9.3} ms · final-packet assembly CPU {:>9.3} ms · {status}",
                style.dim,
                style.reset,
                duration_or_na(fields.u64("access_unit_gap_us").unwrap_or_default()),
                duration_or_na(fields.u64("output_gap_us").unwrap_or_default()),
                micros_to_ms(fields.u64("assembly_age_us").unwrap_or_default()),
                micros_to_ms(fields.u64("packet_assemble_us").unwrap_or_default()),
            )?;
            let attempts = fields.u64("dave_decrypt_attempts").unwrap_or_default();
            if attempts > 0 {
                write!(
                    output,
                    " · DAVE {attempts} attempt{}",
                    if attempts == 1 { "" } else { "s" },
                )?;
            }
            writeln!(output)
        }
        _ => Ok(()),
    }
}

fn render_stage(
    fields: &TimingFields,
    pipeline_us: u64,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let total_us = stage_total_us(fields).unwrap_or_default();
    writeln!(
        output,
        "{}│{}  {}{:<34}{} {:>9.3} ms",
        style.dim,
        style.reset,
        style.bold,
        stage_label(fields.stage()),
        style.reset,
        micros_to_ms(total_us),
    )?;

    render_component_rows(fields, total_us, "│    ", style, output)?;

    render_stage_details(fields, pipeline_us, style, output)
}

fn render_component_rows(
    fields: &TimingFields,
    total_us: u64,
    prefix: &str,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let hottest = components(fields.stage())
        .iter()
        .filter_map(|component| {
            fields
                .u64(component.field)
                .map(|duration| (component.field, duration))
        })
        .max_by_key(|(_, duration)| *duration)
        .map(|(field, _)| field);
    for component in components(fields.stage()) {
        let Some(duration_us) = fields.u64(component.field) else {
            continue;
        };
        let denominator = total_us.max(1);
        let percent = percentage(duration_us, denominator);
        let is_hot = component.actionable
            && hottest == Some(component.field)
            && duration_us >= 1_000
            && percent >= 50.0;
        let color = if is_hot { style.red } else { style.dim };
        writeln!(
            output,
            "{}{prefix}├─{} {:<32} {color}{:>9.3} ms{}  {:>5.1}%{}",
            style.dim,
            style.reset,
            component.label,
            micros_to_ms(duration_us),
            style.reset,
            percent,
            if is_hot { "  ← HOT" } else { "" },
        )?;
    }
    Ok(())
}

fn render_stage_details(
    fields: &TimingFields,
    pipeline_us: u64,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    match fields.stage() {
        "screen.capture" | "webcam.capture" => {
            let source_width = fields.u64("source_width").unwrap_or_default();
            let source_height = fields.u64("source_height").unwrap_or_default();
            let output_width = fields.u64("output_width").unwrap_or_default();
            let output_height = fields.u64("output_height").unwrap_or_default();
            let format = fields.text("source_format").unwrap_or("pixels");
            writeln!(
                output,
                "{}│{}    ╰─ {}×{} {format} ({}) → {}×{} ({})",
                style.dim,
                style.reset,
                source_width,
                source_height,
                bytes(fields.u64("source_bytes").unwrap_or_default()),
                output_width,
                output_height,
                bytes(fields.u64("output_bytes").unwrap_or_default()),
            )?;
        }
        "channel.consume" | "channel.try_consume" => {
            if let Some(queue_age_us) = fields.u64("queue_age_us") {
                writeln!(
                    output,
                    "{}│{}    ╰─ frame waited {:>9.3} ms in latest-frame queue",
                    style.dim,
                    style.reset,
                    micros_to_ms(queue_age_us),
                )?;
            }
        }
        "h264.encode" | "av1.encode" => {
            writeln!(
                output,
                "{}│{}    ╰─ {}×{} · {} raw → {} encoded",
                style.dim,
                style.reset,
                fields.u64("width").unwrap_or_default(),
                fields.u64("height").unwrap_or_default(),
                bytes(fields.u64("input_bytes").unwrap_or_default()),
                bytes(fields.u64("output_bytes").unwrap_or_default()),
            )?;
        }
        "outgoing.transport" => {
            let packets_sent = fields.u64("packets_sent").unwrap_or_default();
            let attempts = fields.u64("dave_encrypt_attempts").unwrap_or_default();
            write!(
                output,
                "{}│{}    ╰─ {packets_sent} RTP packet{} · {} on wire",
                style.dim,
                style.reset,
                if packets_sent == 1 { "" } else { "s" },
                bytes(fields.u64("wire_bytes").unwrap_or_default()),
            )?;
            if attempts > 0 {
                write!(
                    output,
                    " · DAVE {attempts} attempt{}",
                    if attempts == 1 { "" } else { "s" },
                )?;
            }
            writeln!(output)?;
        }
        "outgoing.complete" => {
            let discord_us = fields.u64("discord_us").unwrap_or_default();
            write!(
                output,
                "{}│{}    ╰─ Discord-side job {:>9.3} ms · pipeline {:>9.3} ms",
                style.dim,
                style.reset,
                micros_to_ms(discord_us),
                micros_to_ms(pipeline_us),
            )?;
            if fields.bool("failed") == Some(true) || fields.optional_debug_text("error").is_some()
            {
                write!(
                    output,
                    " · failed: {}",
                    fields
                        .optional_debug_text("error")
                        .unwrap_or("unknown video send error"),
                )?;
            }
            writeln!(output)?;
        }
        _ => {}
    }
    Ok(())
}

fn micros_to_ms(micros: u64) -> f64 {
    micros as f64 / 1_000.0
}

fn duration_or_na(micros: u64) -> String {
    if micros == 0 {
        "n/a".to_owned()
    } else {
        format!("{:.3} ms", micros_to_ms(micros))
    }
}

fn frames_per_second(interval_us: u64) -> f64 {
    if interval_us == 0 {
        0.0
    } else {
        1_000_000.0 / interval_us as f64
    }
}

fn percentage(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

fn bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.2} KiB", bytes as f64 / KIB),
        _ => format!("{:.2} MiB", bytes as f64 / MIB),
    }
}

struct Styles<'a> {
    reset: &'a str,
    bold: &'a str,
    dim: &'a str,
    cyan: &'a str,
    green: &'a str,
    yellow: &'a str,
    red: &'a str,
}

impl Styles<'static> {
    fn new(ansi: bool) -> Self {
        if ansi {
            Self {
                reset: RESET,
                bold: BOLD,
                dim: DIM,
                cyan: CYAN,
                green: GREEN,
                yellow: YELLOW,
                red: RED,
            }
        } else {
            Self {
                reset: "",
                bold: "",
                dim: "",
                cyan: "",
                green: "",
                yellow: "",
                red: "",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(stage: &'static str, values: &[(&'static str, u64)]) -> TimingFields {
        let mut fields = TimingFields::default();
        fields.insert("stage", FieldValue::Text(stage.to_owned()));
        fields.insert("frame_id", FieldValue::U64(42));
        for (name, value) in values {
            fields.insert(name, FieldValue::U64(*value));
        }
        fields
    }

    fn sample_report() -> FrameReport {
        let mut report = FrameReport {
            frame_id: 42,
            stages: Vec::new(),
        };
        report.record(fields(
            "screen.capture",
            &[
                ("elapsed_us", 20_281),
                ("wait_us", 19_198),
                ("normalize_us", 1_083),
            ],
        ));
        report.record(fields(
            "h264.encode",
            &[
                ("total_us", 392_365),
                ("rgb_to_yuv_us", 1_026),
                ("codec_us", 391_333),
                ("output_copy_us", 4),
            ],
        ));
        report.record(fields(
            "outgoing.transport",
            &[
                ("total_us", 2_000),
                ("dave_encrypt_us", 1_500),
                ("dave_preflight_us", 400),
                ("dave_transform_us", 1_100),
                ("dave_encrypt_attempts", 2),
                ("packets_sent", 3),
                ("wire_bytes", 3_600),
            ],
        ));
        let mut complete = fields(
            "outgoing.complete",
            &[
                ("pipeline_elapsed_us", 412_085),
                ("frame_budget_us", 33_333),
                ("discord_us", 392_500),
                ("job_us", 392_490),
            ],
        );
        complete.insert("kind", FieldValue::Text("Screen".to_owned()));
        complete.insert("emitted", FieldValue::Bool(true));
        report.record(complete);
        report.stages.sort_by_key(|stage| stage_rank(stage.stage()));
        report
    }

    fn healthy_report() -> FrameReport {
        let mut report = sample_report();
        let complete = report
            .stages
            .iter_mut()
            .find(|stage| stage.stage() == "outgoing.complete")
            .unwrap();
        complete.insert("pipeline_elapsed_us", FieldValue::U64(30_000));
        complete.insert("discord_us", FieldValue::U64(20_000));
        complete.insert("frame_budget_us", FieldValue::U64(33_333));
        complete.insert("over_budget", FieldValue::Bool(false));
        complete.insert("failed", FieldValue::Bool(false));
        complete.insert("error", FieldValue::Text("None".to_owned()));
        let transport = report
            .stages
            .iter_mut()
            .find(|stage| stage.stage() == "outgoing.transport")
            .unwrap();
        transport.insert("output_gap_us", FieldValue::U64(33_333));
        transport.insert("keyframe", FieldValue::Bool(false));
        report
    }

    fn report_stage_mut<'a>(report: &'a mut FrameReport, name: &str) -> &'a mut TimingFields {
        report
            .stages
            .iter_mut()
            .find(|stage| stage.stage() == name)
            .unwrap()
    }

    #[test]
    fn report_separates_capture_call_age_from_budgeted_frame_work() {
        let mut output = String::new();

        render_report(&sample_report(), 11, 25.2, false, &mut output).unwrap();

        assert!(output.contains("CAPTURE CALL → SEND"));
        assert!(output.contains("412.085 ms"));
        assert!(output.contains("DISCORD FRAME WORK"));
        assert!(output.contains("392.500 ms"));
        assert!(output.contains("HOTTEST ACTIONABLE STEP  OpenH264 encode core"));
        assert!(output.contains("391.333 ms"));
        assert!(output.contains("DAVE allocation + preflight"));
        assert!(output.contains("DAVE transform + AES-GCM"));
        assert!(output.contains("DAVE 2 attempts"));
        assert!(output.contains("11 stale frames replaced"));
        assert!(!output.contains("elapsed_us"));
    }

    #[test]
    fn capture_wait_does_not_mark_fast_discord_work_over_budget() {
        let mut report = sample_report();
        let complete = report
            .stages
            .iter_mut()
            .find(|stage| stage.stage() == "outgoing.complete")
            .unwrap();
        complete.insert("pipeline_elapsed_us", FieldValue::U64(298_037));
        complete.insert("discord_us", FieldValue::U64(3_329));
        complete.insert("frame_budget_us", FieldValue::U64(33_333));
        complete.insert("over_budget", FieldValue::Bool(false));
        let mut output = String::new();

        render_report(&report, 0, 1.0, false, &mut output).unwrap();

        assert!(output.contains("CAPTURE CALL → SEND"));
        assert!(output.contains("298.037 ms"));
        assert!(output.contains("DISCORD FRAME WORK"));
        assert!(output.contains("3.329 ms"));
        assert!(output.contains("0.1× OF BUDGET"));
        assert!(output.contains("30.0 FPS · 33.333 ms"));
        assert!(!output.contains("OVER BUDGET"));
        assert!(!output.contains("BOTTLENECK"));
    }

    #[test]
    fn healthy_reports_are_sampled_after_the_first() {
        let report = healthy_report();
        let mut state = ReportState::default();
        let mut rendered = Vec::new();

        for report_index in 1..=60 {
            if state.should_render(&report, 0) {
                rendered.push(report_index);
            }
        }

        assert_eq!(rendered, [1, 30, 60]);
    }

    #[test]
    fn anomalies_bypass_healthy_report_sampling() {
        let healthy = healthy_report();
        assert!(!report_is_anomalous(&healthy, 0));
        assert!(report_is_anomalous(&healthy, 1));

        let mut failed = healthy.clone();
        report_stage_mut(&mut failed, "outgoing.complete").insert("failed", FieldValue::Bool(true));
        assert!(report_is_anomalous(&failed, 0));

        let mut over_budget = healthy.clone();
        report_stage_mut(&mut over_budget, "outgoing.complete")
            .insert("over_budget", FieldValue::Bool(true));
        assert!(report_is_anomalous(&over_budget, 0));

        let mut stalled = healthy.clone();
        report_stage_mut(&mut stalled, "outgoing.transport")
            .insert("output_gap_us", FieldValue::U64(66_667));
        assert!(report_is_anomalous(&stalled, 0));

        let mut keyframe = healthy.clone();
        report_stage_mut(&mut keyframe, "outgoing.transport")
            .insert("keyframe", FieldValue::Bool(true));
        assert!(report_is_anomalous(&keyframe, 0));

        let mut rebuilt = healthy.clone();
        let mut av1 = fields(
            "av1.encode",
            &[("total_us", 10_000), ("need_more_data_streak", 0)],
        );
        av1.insert("context_rebuilt", FieldValue::Bool(true));
        rebuilt.record(av1);
        assert!(report_is_anomalous(&rebuilt, 0));

        let mut buffered = healthy;
        buffered.record(fields(
            "av1.encode",
            &[("total_us", 10_000), ("need_more_data_streak", 3)],
        ));
        assert!(report_is_anomalous(&buffered, 0));
    }

    #[test]
    fn delayed_av1_output_preserves_buffering_and_rebuild_evidence() {
        let mut report = FrameReport::default();
        let mut buffered = fields(
            "av1.encode",
            &[("need_more_data_streak", 4), ("context_rebuild_us", 9_000)],
        );
        buffered.insert("context_rebuilt", FieldValue::Bool(true));
        report.record(buffered);
        let mut emitted = fields(
            "av1.encode",
            &[("need_more_data_streak", 0), ("context_rebuild_us", 0)],
        );
        emitted.insert("context_rebuilt", FieldValue::Bool(false));

        report.record(emitted);

        let av1 = report.stage("av1.encode").unwrap();
        assert_eq!(av1.u64("need_more_data_streak"), Some(4));
        assert_eq!(av1.bool("context_rebuilt"), Some(true));
        assert_eq!(av1.u64("context_rebuild_us"), Some(9_000));
    }

    #[test]
    fn av1_report_exposes_output_gap_and_frame_provenance() {
        let mut report = healthy_report();
        report.stages.retain(|stage| stage.stage() != "h264.encode");
        let mut av1 = fields(
            "av1.encode",
            &[
                ("total_us", 16_700),
                ("state_setup_us", 12_000),
                ("frame_alloc_us", 100),
                ("rgb_to_yuv_us", 1_300),
                ("send_frame_us", 300),
                ("receive_packet_us", 3_000),
                ("submitted_sequence", 106),
                ("emitted_sequence", 100),
                ("lag_frames", 6),
                ("encoder_residence_us", 188_000),
                ("context_generation", 3),
                ("context_rebuild_us", 12_000),
                ("emitted_context_generation", 3),
                ("emitted_context_rebuild_us", 12_000),
                ("need_more_data_streak", 5),
                ("encoded_loop_count", 2),
                ("width", 1_280),
                ("height", 720),
                ("input_bytes", 2_764_800),
                ("output_bytes", 12_288),
            ],
        );
        av1.insert("submitted_forced_keyframe", FieldValue::Bool(true));
        av1.insert("emitted_forced_keyframe", FieldValue::Bool(true));
        av1.insert("emitted_keyframe", FieldValue::Bool(true));
        av1.insert("context_rebuilt", FieldValue::Bool(false));
        av1.insert("emitted_context_rebuilt", FieldValue::Bool(true));
        report.record(av1);
        let transport = report_stage_mut(&mut report, "outgoing.transport");
        transport.insert("output_gap_us", FieldValue::U64(100_000));
        transport.insert("keyframe", FieldValue::Bool(true));
        report.stages.sort_by_key(|stage| stage_rank(stage.stage()));
        let mut output = String::new();

        render_report(&report, 0, 2.0, false, &mut output).unwrap();

        assert!(output.contains("WIRE OUTPUT GAP"));
        assert!(output.contains("100.000 ms"));
        assert!(output.contains("AV1 PROVENANCE"));
        assert!(output.contains("submit #106 → emit #100"));
        assert!(output.contains("lag 6 frames"));
        assert!(output.contains("residence 188.000 ms"));
        assert!(output.contains("KEYFRAME"));
        assert!(output.contains("force current=true emitted=true"));
        assert!(output.contains("context #3 rebuilt in 12.000 ms"));
        assert!(output.contains("buffered streak 5"));
        assert!(output.contains("encoded loop count 2"));
        assert!(output.contains("encoder state setup"));
        assert!(output.contains("rav1e frame allocation"));
        assert!(output.contains("rav1e frame submission"));
        assert!(output.contains("rav1e packet production"));
    }

    #[test]
    fn failed_completion_prints_error_text() {
        let mut report = healthy_report();
        let complete = report_stage_mut(&mut report, "outgoing.complete");
        complete.insert("emitted", FieldValue::Bool(false));
        complete.insert("failed", FieldValue::Bool(true));
        complete.insert(
            "error",
            FieldValue::Text("Some(udp send failed)".to_owned()),
        );
        let mut output = String::new();

        render_report(&report, 0, 2.0, false, &mut output).unwrap();

        assert!(output.contains("FAILED  udp send failed"));
        assert!(output.contains("failed: udp send failed"));
    }

    #[test]
    fn outgoing_control_is_compact_without_fake_duration() {
        let mut control = fields("outgoing.control", &[]);
        control.insert("kind", FieldValue::Text("Screen".to_owned()));
        control.insert("action", FieldValue::Text("keyframe_pending".to_owned()));
        control.insert("codec", FieldValue::Text("Av1".to_owned()));
        control.insert("reason", FieldValue::Text("periodic".to_owned()));
        control.insert("emitted", FieldValue::Bool(false));
        control.insert("keyframe", FieldValue::Bool(false));
        let mut output = String::new();

        render_single_stage(&control, 2.0, false, &mut output).unwrap();

        assert!(output.contains("VIDEO CONTROL · Screen · keyframe_pending"));
        assert!(output.contains("codec Av1 · reason periodic · frame #42"));
        assert!(output.contains("emitted false · keyframe false"));
        assert!(!output.contains("0.000 ms"));
    }

    #[test]
    fn dave_transition_control_exposes_gate_lifecycle() {
        let mut control = fields("outgoing.control", &[("elapsed_us", 2_500)]);
        control.insert("kind", FieldValue::Text("DAVE".to_owned()));
        control.insert(
            "action",
            FieldValue::Text("dave_transition_executed".to_owned()),
        );
        control.insert("transition_id", FieldValue::U64(17));
        control.insert("protocol_version", FieldValue::U64(1));
        control.insert("pending_transitions", FieldValue::U64(0));
        let mut output = String::new();

        render_single_stage(&control, 2.0, false, &mut output).unwrap();

        assert!(output.contains("DAVE · dave_transition_executed"));
        assert!(output.contains("transition #17 · protocol v1 · pending 0"));
        assert!(output.contains("2.500 ms"));
    }

    #[test]
    fn decoder_report_splits_decode_allocation_and_conversion() {
        let mut decode = fields(
            "h264.decode",
            &[
                ("total_us", 4_200),
                ("decoder_id", 7),
                ("access_unit_bytes", 36_640),
                ("width", 1_280),
                ("height", 720),
                ("output_bytes", 3_686_400),
                ("codec_us", 2_100),
                ("rgba_alloc_us", 400),
                ("yuv_to_rgba_us", 1_600),
                ("rgba_us", 2_000),
            ],
        );
        decode.insert("conversion_path", FieldValue::Text("f32x8 SIMD".to_owned()));
        decode.insert(
            "decoder_thread_mode",
            FieldValue::Text("single-thread (OpenH264 num_threads=0)".to_owned()),
        );
        decode.insert("emitted", FieldValue::Bool(true));
        decode.insert("failed", FieldValue::Bool(false));
        let mut output = String::new();

        render_single_stage(&decode, 2.0, false, &mut output).unwrap();

        assert!(output.contains("OpenH264 decode"));
        assert!(output.contains("RGBA allocation + zero-fill"));
        assert!(output.contains("YUV → RGBA conversion"));
        assert!(output.contains("f32x8 SIMD"));
        assert!(output.contains("single-thread (OpenH264 num_threads=0)"));
        assert!(output.contains("decoder #7"));
        assert!(!output.contains("rgba_us"));

        decode.insert("emitted", FieldValue::Bool(false));
        decode.insert("failed", FieldValue::Bool(true));
        decode.insert("error", FieldValue::Text("corrupt bitstream".to_owned()));
        let mut failed_output = String::new();
        render_single_stage(&decode, 2.0, false, &mut failed_output).unwrap();
        assert!(failed_output.contains("failed: corrupt bitstream"));
        assert!(!failed_output.contains("0×0 RGBA"));
    }

    #[test]
    fn incoming_report_shows_sender_and_local_cadence() {
        let mut incoming = fields(
            "incoming.receive",
            &[
                ("total_us", 4_000),
                ("ssrc", 123),
                ("decoder_id", 7),
                ("stream_frame_index", 8),
                ("rtp_delta_ticks", 4_500),
                ("source_interval_us", 50_000),
                ("access_unit_gap_us", 50_250),
                ("output_gap_us", 100_500),
                ("assembly_age_us", 2_000),
                ("packet_assemble_us", 40),
                ("receive_bookkeeping_us", 100),
                ("dave_lock_wait_us", 10),
                ("dave_decrypt_us", 90),
                ("dave_decrypt_preflight_us", 20),
                ("dave_decrypt_transform_us", 70),
                ("dave_decrypt_attempts", 1),
                ("decode_us", 3_800),
            ],
        );
        incoming.insert("emitted", FieldValue::Bool(true));
        let mut output = String::new();

        render_single_stage(&incoming, 2.0, false, &mut output).unwrap();

        assert!(output.contains("SSRC 123"));
        assert!(output.contains("decoder #7"));
        assert!(output.contains("receive bookkeeping/NACK"));
        assert!(output.contains("DAVE decrypt allocation/preflight"));
        assert!(output.contains("DAVE decrypt transform"));
        assert!(output.contains("DAVE 1 attempt"));
        assert!(output.contains("RTP Δ 4500 ticks"));
        assert!(output.contains("observed RTP AU cadence 20.0 FPS"));
        assert!(output.contains("AU gap 50.250 ms"));
        assert!(output.contains("output gap 100.500 ms"));

        incoming.insert("source_interval_us", FieldValue::U64(0));
        incoming.insert("access_unit_gap_us", FieldValue::U64(0));
        incoming.insert("output_gap_us", FieldValue::U64(0));
        let mut first_output = String::new();
        render_single_stage(&incoming, 2.0, false, &mut first_output).unwrap();
        assert!(first_output.contains("observed RTP AU cadence n/a (first sample)"));
        assert!(first_output.contains("AU gap n/a · output gap n/a"));
        assert!(!first_output.contains("0.0 FPS"));
    }

    #[test]
    fn ansi_is_emitted_only_when_enabled() {
        let mut plain = String::new();
        let mut colored = String::new();

        render_report(&sample_report(), 0, 1.0, false, &mut plain).unwrap();
        render_report(&sample_report(), 0, 1.0, true, &mut colored).unwrap();

        assert!(!plain.contains("\x1b["));
        assert!(colored.contains("\x1b[31m"));
    }

    #[test]
    fn state_evicts_old_incomplete_frames() {
        let mut state = ReportState::default();
        for frame_id in 1..=MAX_PENDING_FRAMES as u64 + 20 {
            state.record(frame_id, fields("channel.publish", &[]));
        }

        assert_eq!(state.frames.len(), MAX_PENDING_FRAMES);
        assert!(!state.frames.contains_key(&1));
    }

    #[test]
    fn inbound_decoder_ids_are_not_correlated_with_outbound_frames() {
        let inbound = fields("h264.decode", &[("frame_id", 42)]);
        assert_eq!(correlated_frame_id(&inbound), None);

        let mut av1 = fields("av1.encode", &[("frame_id", 44), ("emitted_frame_id", 42)]);
        av1.insert("emitted", FieldValue::Bool(true));
        assert_eq!(correlated_frame_id(&av1), Some(42));
    }
}
