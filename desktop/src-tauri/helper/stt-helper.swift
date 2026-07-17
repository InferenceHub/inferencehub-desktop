// ih-stt-helper — native macOS speech-to-text sidecar for the InferenceHub desktop app.
//
// Engine: whisper.cpp (Metal) with the small.en-q5_1 model, downloaded on first
// use to ~/Library/Application Support/InferenceHub/models/ (~181 MB, cached
// forever). Replaced SFSpeechRecognizer in v0.2.11 — Apple's recognizer got
// too many words wrong, whisper small.en is markedly more accurate and fully
// offline after the one-time download.
//
// Protocol: one JSON object per stdout line —
//   {"type":"downloading","percent":"42"}   model download progress (first run)
//   {"type":"ready"}                        engine running, mic live
//   {"type":"partial","text":"..."}         rolling hypothesis for the current utterance
//   {"type":"final","text":"..."}           finalized utterance (silence or window cap)
//   {"type":"error","message":"..."}        fatal — helper exits 1 afterwards
//
// Lifecycle: runs until SIGTERM/SIGINT (Rust kills it) or stdin closes
// (parent died).

import AVFoundation
import Foundation
import whisper

// MARK: - protocol plumbing

func emit(_ obj: [String: String]) {
    guard let data = try? JSONSerialization.data(withJSONObject: obj),
          let line = String(data: data, encoding: .utf8)
    else { return }
    print(line)
    fflush(stdout)
}

func fail(_ message: String) -> Never {
    emit(["type": "error", "message": message])
    exit(1)
}

// MARK: - model download

let MODEL_URL = URL(
    string: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en-q5_1.bin")!

func modelPath() -> URL {
    let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    return base.appendingPathComponent("InferenceHub/models/ggml-small.en-q5_1.bin")
}

func ensureModel() -> URL {
    let dest = modelPath()
    if FileManager.default.fileExists(atPath: dest.path) {
        return dest
    }
    try? FileManager.default.createDirectory(
        at: dest.deletingLastPathComponent(), withIntermediateDirectories: true)

    emit(["type": "downloading", "percent": "0"])
    let sema = DispatchSemaphore(value: 0)
    var downloadError: String?

    final class Delegate: NSObject, URLSessionDownloadDelegate {
        let dest: URL
        let done: (String?) -> Void
        var lastPercent = -1
        init(dest: URL, done: @escaping (String?) -> Void) {
            self.dest = dest
            self.done = done
        }
        func urlSession(
            _ session: URLSession, downloadTask: URLSessionDownloadTask,
            didWriteData: Int64, totalBytesWritten: Int64, totalBytesExpectedToWrite: Int64
        ) {
            guard totalBytesExpectedToWrite > 0 else { return }
            let pct = Int(totalBytesWritten * 100 / totalBytesExpectedToWrite)
            if pct >= lastPercent + 2 {
                lastPercent = pct
                emit(["type": "downloading", "percent": String(pct)])
            }
        }
        func urlSession(
            _ session: URLSession, downloadTask: URLSessionDownloadTask,
            didFinishDownloadingTo location: URL
        ) {
            do {
                // Atomic move so a half-written file never passes the exists check.
                if FileManager.default.fileExists(atPath: dest.path) {
                    try FileManager.default.removeItem(at: dest)
                }
                try FileManager.default.moveItem(at: location, to: dest)
                done(nil)
            } catch {
                done("Could not save model: \(error.localizedDescription)")
            }
        }
        func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
            if let error { done("Model download failed: \(error.localizedDescription)") }
        }
    }

    let delegate = Delegate(dest: dest) { err in
        downloadError = err
        sema.signal()
    }
    let config = URLSessionConfiguration.default
    config.timeoutIntervalForRequest = 120  // idle timeout between chunks
    config.timeoutIntervalForResource = 3600  // whole-file budget (181 MB on slow links)
    let session = URLSession(configuration: config, delegate: delegate, delegateQueue: nil)
    session.downloadTask(with: MODEL_URL).resume()
    sema.wait()
    if let downloadError { fail(downloadError) }
    emit(["type": "downloading", "percent": "100"])
    return dest
}

// MARK: - whisper engine

final class WhisperEngine {
    private let ctx: OpaquePointer

    init(modelFile: URL) {
        var cparams = whisper_context_default_params()
        cparams.use_gpu = true  // Metal
        guard let ctx = whisper_init_from_file_with_params(modelFile.path, cparams) else {
            fail("Could not load the speech model")
        }
        self.ctx = ctx
    }

    /// Transcribe a 16 kHz mono Float32 buffer; returns joined segment text.
    func transcribe(_ samples: [Float]) -> String {
        var params = whisper_full_default_params(WHISPER_SAMPLING_GREEDY)
        params.print_realtime = false
        params.print_progress = false
        params.print_timestamps = false
        params.print_special = false
        params.no_context = true
        params.single_segment = false
        params.suppress_blank = true
        params.language = ("en" as NSString).utf8String
        params.n_threads = Int32(max(2, ProcessInfo.processInfo.activeProcessorCount - 2))

        let rc = samples.withUnsafeBufferPointer { buf in
            whisper_full(ctx, params, buf.baseAddress, Int32(buf.count))
        }
        guard rc == 0 else { return "" }

        var text = ""
        for i in 0..<whisper_full_n_segments(ctx) {
            if let seg = whisper_full_get_segment_text(ctx, i) {
                text += String(cString: seg)
            }
        }
        return text.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

// MARK: - streaming transcriber (16 kHz samples in → stepped whisper passes out)

final class Transcriber {
    private let whisperEngine: WhisperEngine
    private let queue = DispatchQueue(label: "stt.whisper")  // serial: one pass at a time

    static let sampleRate = 16000.0
    private let sampleRate = Transcriber.sampleRate
    private let bufferLock = NSLock()
    private var utterance: [Float] = []  // current utterance, 16 kHz mono

    private var whisperBusy = false
    private var silentFrames = 0
    private var voicedInUtterance = false

    // Tuning
    private let stepSeconds = 1.5      // partial pass cadence
    private let silenceSeconds = 1.2   // this much quiet finalizes the utterance
    private let maxUtteranceSeconds = 25.0
    private let rmsThreshold: Float = 0.008

    init(whisperEngine: WhisperEngine) {
        self.whisperEngine = whisperEngine
    }

    func start() {
        // Partial-pass timer.
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + stepSeconds, repeating: stepSeconds)
        timer.setEventHandler { [weak self] in self?.step() }
        timer.resume()
        // Keep a strong ref for process lifetime.
        _timer = timer
        // "ready" is emitted by the audio SOURCE once capture is actually live
        // (the system source starts asynchronously and may still fail on TCC).
    }

    private var _timer: DispatchSourceTimer?

    /// Feed a chunk of 16 kHz mono Float32 samples (from any audio source).
    func ingestSamples(_ samples: [Float]) {
        guard !samples.isEmpty else { return }
        // RMS VAD on this chunk.
        let rms = sqrt(samples.reduce(0) { $0 + $1 * $1 } / Float(samples.count))
        let voiced = rms >= rmsThreshold

        bufferLock.lock()
        if voiced || voicedInUtterance {
            utterance.append(contentsOf: samples)
        }
        if voiced {
            voicedInUtterance = true
            silentFrames = 0
        } else if voicedInUtterance {
            silentFrames += samples.count
        }
        let silentSecs = Double(silentFrames) / sampleRate
        let utterSecs = Double(utterance.count) / sampleRate
        let shouldFinalize =
            voicedInUtterance
            && (silentSecs >= silenceSeconds || utterSecs >= maxUtteranceSeconds)
        bufferLock.unlock()

        if shouldFinalize {
            queue.async { [weak self] in self?.finalize() }
        }
    }

    /// Periodic partial pass over the in-flight utterance.
    private func step() {
        if whisperBusy { return }
        bufferLock.lock()
        guard voicedInUtterance, !utterance.isEmpty else {
            bufferLock.unlock()
            return
        }
        // Cap the partial window so passes stay fast; the final pass uses it all.
        let windowLimit = Int(sampleRate * 10)
        let samples = Array(utterance.suffix(windowLimit))
        bufferLock.unlock()

        // whisper needs at least ~1s of audio to behave.
        guard samples.count >= Int(sampleRate) else { return }
        whisperBusy = true
        let text = whisperEngine.transcribe(samples)
        whisperBusy = false
        if !text.isEmpty {
            emit(["type": "partial", "text": text])
        }
    }

    /// Silence (or window cap) hit: full-utterance pass, emit final, reset.
    private func finalize() {
        bufferLock.lock()
        let samples = utterance
        utterance = []
        voicedInUtterance = false
        silentFrames = 0
        bufferLock.unlock()

        guard samples.count >= Int(sampleRate / 2) else { return }
        whisperBusy = true
        let text = whisperEngine.transcribe(samples)
        whisperBusy = false
        if !text.isEmpty {
            emit(["type": "final", "text": text])
        }
    }
}

// MARK: - audio sources

/// Converts arbitrary-format PCM buffers to 16 kHz mono Float32 and feeds the
/// transcriber. One instance per source stream format.
final class SampleFeeder {
    private let transcriber: Transcriber
    private let outFormat: AVAudioFormat
    private var converter: AVAudioConverter?
    private var inFormat: AVAudioFormat?

    init(transcriber: Transcriber) {
        self.transcriber = transcriber
        guard
            let f = AVAudioFormat(
                commonFormat: .pcmFormatFloat32, sampleRate: Transcriber.sampleRate,
                channels: 1, interleaved: false)
        else { fail("Could not configure audio conversion") }
        self.outFormat = f
    }

    func feed(_ buffer: AVAudioPCMBuffer) {
        if converter == nil || inFormat != buffer.format {
            inFormat = buffer.format
            converter = AVAudioConverter(from: buffer.format, to: outFormat)
        }
        guard let converter else { return }
        let ratio = Transcriber.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 16
        guard let out = AVAudioPCMBuffer(pcmFormat: outFormat, frameCapacity: capacity) else {
            return
        }
        var consumed = false
        var convError: NSError?
        converter.convert(to: out, error: &convError) { _, status in
            if consumed {
                status.pointee = .noDataNow
                return nil
            }
            consumed = true
            status.pointee = .haveData
            return buffer
        }
        guard convError == nil, let ch = out.floatChannelData, out.frameLength > 0 else { return }
        transcriber.ingestSamples(
            Array(UnsafeBufferPointer(start: ch[0], count: Int(out.frameLength))))
    }
}

/// Microphone via AVAudioEngine input tap (the original v0.2.10 path).
final class MicSource {
    private let engine = AVAudioEngine()
    private let feeder: SampleFeeder

    init(feeder: SampleFeeder) {
        self.feeder = feeder
    }

    func start() {
        let input = engine.inputNode
        let inFormat = input.outputFormat(forBus: 0)
        guard inFormat.sampleRate > 0 else { fail("No audio input device") }
        input.installTap(onBus: 0, bufferSize: 4096, format: inFormat) { [feeder] buffer, _ in
            feeder.feed(buffer)
        }
        engine.prepare()
        do { try engine.start() } catch {
            fail("Could not start audio engine: \(error.localizedDescription)")
        }
        emit(["type": "ready"])
    }
}

#if canImport(ScreenCaptureKit)
import CoreMedia
import ScreenCaptureKit

/// System-wide audio via ScreenCaptureKit — hears the meeting even on
/// headphones (any app: Zoom, Teams, browser). Requires the Screen & System
/// Audio Recording permission; video frames are configured minimal and dropped.
final class SystemAudioSource: NSObject, SCStreamOutput, SCStreamDelegate {
    private let feeder: SampleFeeder
    private var stream: SCStream?

    init(feeder: SampleFeeder) {
        self.feeder = feeder
    }

    func start() {
        Task {
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(
                    false, onScreenWindowsOnly: true)
                guard let display = content.displays.first else {
                    fail("No display found for system audio capture")
                }
                // Exclude our own app so injected TTS/system sounds from
                // InferenceHub itself don't feed back into the transcript.
                let ownApps = content.applications.filter {
                    $0.bundleIdentifier.hasPrefix("tech.inferencehub")
                }
                let filter = SCContentFilter(
                    display: display, excludingApplications: ownApps, exceptingWindows: [])

                let config = SCStreamConfiguration()
                config.capturesAudio = true
                config.sampleRate = Int(Transcriber.sampleRate)
                config.channelCount = 1
                config.excludesCurrentProcessAudio = true
                // Video is mandatory in the stream; keep it as cheap as possible
                // and drop every frame in the output handler.
                config.width = 2
                config.height = 2
                config.minimumFrameInterval = CMTime(value: 1, timescale: 1)

                let stream = SCStream(filter: filter, configuration: config, delegate: self)
                try stream.addStreamOutput(
                    self, type: .audio, sampleHandlerQueue: DispatchQueue(label: "stt.sysaudio"))
                try await stream.startCapture()
                self.stream = stream
                emit(["type": "ready"])
            } catch {
                fail(
                    "System audio permission needed — allow InferenceHub under "
                        + "System Settings → Privacy & Security → Screen & System Audio Recording "
                        + "(\(error.localizedDescription))")
            }
        }
    }

    func stream(
        _ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of type: SCStreamOutputType
    ) {
        guard type == .audio, sampleBuffer.isValid else { return }
        guard let pcm = sampleBuffer.toPCMBuffer() else { return }
        feeder.feed(pcm)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        fail("System audio capture stopped: \(error.localizedDescription)")
    }
}

extension CMSampleBuffer {
    /// CMSampleBuffer (audio) → AVAudioPCMBuffer, preserving the stream format.
    func toPCMBuffer() -> AVAudioPCMBuffer? {
        guard let desc = CMSampleBufferGetFormatDescription(self),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(desc)
        else { return nil }
        let frames = AVAudioFrameCount(CMSampleBufferGetNumSamples(self))
        guard frames > 0,
              let format = AVAudioFormat(streamDescription: asbd),
              let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames)
        else { return nil }
        pcm.frameLength = frames
        let status = CMSampleBufferCopyPCMDataIntoAudioBufferList(
            self, at: 0, frameCount: Int32(frames),
            into: pcm.mutableAudioBufferList)
        return status == noErr ? pcm : nil
    }
}
#endif

// MARK: - lifecycle

signal(SIGINT) { _ in exit(0) }
signal(SIGTERM) { _ in exit(0) }

// Exit when the parent closes stdin (parent process died without SIGTERM).
DispatchQueue.global().async {
    while readLine(strippingNewline: true) != nil {}
    exit(0)
}

// --source mic|system (default mic); passed by the Rust shell from the chat
// page's ih-stt.localhost/start?source=… sentinel.
var source = "mic"
if let i = CommandLine.arguments.firstIndex(of: "--source"),
   CommandLine.arguments.count > i + 1 {
    source = CommandLine.arguments[i + 1]
}

let model = ensureModel()
let whisperEngine = WhisperEngine(modelFile: model)
let transcriber = Transcriber(whisperEngine: whisperEngine)
let feeder = SampleFeeder(transcriber: transcriber)

// Keep sources alive for the process lifetime.
var micSource: MicSource?
#if canImport(ScreenCaptureKit)
var systemSource: SystemAudioSource?
#endif

DispatchQueue.main.async {
    switch source {
    case "system":
        #if canImport(ScreenCaptureKit)
        let s = SystemAudioSource(feeder: feeder)
        systemSource = s
        s.start()
        #else
        fail("System audio capture not supported in this build")
        #endif
    default:
        let m = MicSource(feeder: feeder)
        micSource = m
        m.start()
    }
    transcriber.start()
}
RunLoop.main.run()
