import SwiftUI

private struct ScheduleRefreshIdentity: Hashable {
    let connection: UUID
    let updated: Date?
}

@MainActor
struct SchedulesView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Host schedules").font(.headline)
                Spacer()
                if model.schedulesLoading { ProgressView().controlSize(.small) }
                Button("Refresh") { Task { await model.loadSchedules() } }
                    .disabled(model.schedulesLoading || model.closing)
            }
            if let engine = model.scheduleEngine { ScheduleEngineView(engine: engine) }
            if let error = model.schedulesError {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.orange).textSelection(.enabled)
            }
            if model.scheduleJobs.isEmpty {
                ContentUnavailableView(model.schedulesLoaded ? "No scheduled jobs" : "Schedules not loaded", systemImage: "calendar", description: Text(model.schedulesLoaded ? "This host has no jobs in the current page." : "Connect to a host with scheduling controls to inspect its jobs."))
            } else {
                List(model.scheduleJobs) { job in
                    Button { model.selectSchedule(job) } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text(job.name).lineLimit(2)
                                Spacer()
                                Text(job.deleted ? "Deleted" : job.enabled ? "Enabled" : "Disabled")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Text(job.frequency).font(.caption).foregroundStyle(.secondary)
                            if let next = job.nextRun {
                                Text("Next: \(next.formatted(date: .abbreviated, time: .shortened))").font(.caption)
                            } else { Text("No next occurrence recorded").font(.caption).foregroundStyle(.secondary) }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Inspect scheduled job \(job.name)")
                }
            }
            HStack {
                Text("\(model.scheduleJobs.count) jobs loaded · read only").font(.caption2).foregroundStyle(.secondary)
                Spacer()
                if model.scheduleCursor != nil, model.scheduleJobs.count < 100 {
                    Button("Load more jobs") { Task { await model.loadSchedules(more: true) } }
                        .disabled(model.schedulesLoading || model.closing)
                }
            }
            if model.schedulesBrowsing {
                Text("Browsing a saved snapshot. Refresh returns to the first page.").font(.caption2).foregroundStyle(.secondary)
            }
            if model.scheduleJobs.count == 100 {
                Text("The view is limited to 100 jobs. Refresh starts again at the first page.").font(.caption2).foregroundStyle(.secondary)
            }
            if let updated = model.schedulesUpdated {
                Text("Schedule snapshot \(updated, style: .time)").font(.caption2).foregroundStyle(.secondary)
            }
        }
        .padding(.top, 8)
        .privacySensitive()
        .task(id: ScheduleRefreshIdentity(connection: model.connectionKey, updated: model.lastRefresh)) { await model.loadSchedules(automatic: true) }
        .sheet(item: $model.selectedSchedule) { job in ScheduleHistoryView(model: model, job: job) }
    }
}

@MainActor
private struct ScheduleEngineView: View {
    let engine: HostScheduleEngine

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Text(engine.stopped ? "Schedule engine stopped" : engine.active ? "Schedule engine active" : "Schedule engine not activated")
                        .font(.caption.bold())
                    Spacer()
                }
                Text("\(engine.blockedCount) blocked jobs · \(engine.pendingDeliveries) pending result deliveries")
                    .font(.caption).foregroundStyle(.secondary)
                if !engine.errors.isEmpty || !engine.blocked.isEmpty {
                    DisclosureGroup("Host diagnostics") {
                        ScrollView {
                            VStack(alignment: .leading, spacing: 6) {
                                ForEach(Array(engine.errors.enumerated()), id: \.offset) { _, error in Text(error) }
                                ForEach(engine.blocked) { blocked in
                                    Text("Job \(blocked.id.string): \(blocked.diagnostic)")
                                }
                            }.frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .font(.caption).textSelection(.enabled).frame(maxHeight: 100)
                    }.font(.caption)
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

private struct ScheduleHistoryRefreshIdentity: Hashable {
    let connection: UUID
    let updated: Date?
    let job: OwnerID
}

@MainActor
private struct ScheduleHistoryView: View {
    @Bindable var model: HostModel
    let job: HostScheduleJob
    @Environment(\.dismiss) private var dismiss

    private var matchingHistory: Bool { model.scheduleHistoryJob == job.id }
    private var runs: [HostScheduleRun] { matchingHistory ? model.scheduleRuns : [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(job.name).font(.title2)
                Spacer()
                if model.scheduleHistoryLoading { ProgressView().controlSize(.small) }
                Button("Refresh") { Task { await model.loadScheduleHistory(job) } }
                    .disabled(model.scheduleHistoryLoading || model.closing)
            }
            Text(job.id.string).font(.caption.monospaced()).textSelection(.enabled)
            Text("\(job.frequency) · generation \(job.generation) · revision \(job.revision)")
                .font(.caption).foregroundStyle(.secondary)
            if matchingHistory, let error = model.scheduleHistoryError {
                Label(error, systemImage: "exclamationmark.triangle").font(.caption).foregroundStyle(.orange)
            }
            if runs.isEmpty {
                ContentUnavailableView(matchingHistory && model.scheduleHistoryLoaded ? "No recorded runs" : "Loading run history", systemImage: "clock", description: Text("Run state comes from the host's durable occurrence history."))
            } else {
                List(runs) { run in ScheduleRunRow(run: run) }
            }
            HStack {
                Text("\(runs.count) runs loaded").font(.caption).foregroundStyle(.secondary)
                if matchingHistory, model.scheduleHistoryCursor != .null, runs.count < 100 {
                    Button("Load older runs") { Task { await model.loadScheduleHistory(job, more: true) } }
                        .disabled(model.scheduleHistoryLoading || model.closing)
                }
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            if model.scheduleHistoryBrowsing {
                Text("Browsing a saved snapshot. Refresh loads the newest runs.").font(.caption2).foregroundStyle(.secondary)
            }
            Text("Inspect Unknown outcomes through the owner CLI before choosing an acknowledgement or another operation.")
                .font(.caption2).foregroundStyle(.secondary)
        }
        .padding(20).frame(width: 660, height: 560)
        .privacySensitive()
        .task(id: ScheduleHistoryRefreshIdentity(connection: model.connectionKey, updated: model.lastRefresh, job: job.id)) { await model.loadScheduleHistory(job, automatic: true) }
    }
}

@MainActor
private struct ScheduleRunRow: View {
    let run: HostScheduleRun

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Label(run.state.capitalized, systemImage: run.unknown ? "exclamationmark.triangle" : "clock")
                    .foregroundStyle(run.unknown ? Color.orange : Color.primary)
                Spacer()
                Text(run.scheduledFor, format: .dateTime).font(.caption)
            }
            Text(run.id.string).font(.caption2.monospaced()).textSelection(.enabled)
            if let summary = run.summary { Text(summary).font(.caption).textSelection(.enabled) }
            if let diagnostic = run.lastDiagnostic, !diagnostic.isEmpty {
                Text(diagnostic).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
            }
            Text("\(run.attempts) attempts · \(run.modelRequests) model requests · \(run.toolCalls) tool calls · \(run.reservedOutputTokens) output tokens reserved")
                .font(.caption2).foregroundStyle(.secondary)
            Text(run.usageKnown ? "Usage counters recorded; they do not prove external effects are known." : "Usage accounting is incomplete.")
                .font(.caption2).foregroundStyle(.secondary)
            if let finished = run.finishedAt { Text("Finished \(finished.formatted(date: .abbreviated, time: .shortened))").font(.caption2) }
        }
        .padding(.vertical, 4)
    }
}
