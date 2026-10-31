import Foundation
import FoundationModels
import TgsortAICore

func eprint(_ s: String) { FileHandle.standardError.write(Data((s + "\n").utf8)) }
func out(_ d: Data) { FileHandle.standardOutput.write(d); FileHandle.standardOutput.write(Data("\n".utf8)) }

func unavailableReason(_ a: SystemLanguageModel.Availability) -> String? {
    switch a {
    case .available: return nil
    case .unavailable(let r):
        switch r {
        case .deviceNotEligible: return "deviceNotEligible"
        case .appleIntelligenceNotEnabled: return "appleIntelligenceNotEnabled"
        case .modelNotReady: return "modelNotReady"
        @unknown default: return "unknown"
        }
    @unknown default: return "unknown"
    }
}

func runCheck() {
    if let r = unavailableReason(SystemLanguageModel.default.availability) {
        out(Data(#"{"available":false,"reason":"\#(r)"}"#.utf8))
    } else {
        out(Data(#"{"available":true}"#.utf8))
    }
}

func describe(_ error: Error) -> String {
    if let e = error as? LanguageModelSession.GenerationError {
        switch e {
        case .exceededContextWindowSize: return "context window exceeded"
        case .guardrailViolation: return "blocked by guardrails"
        default: return "generation error: \(e)"
        }
    }
    return "\(error)"
}

func runClassify() async -> Int32 {
    let input = FileHandle.standardInput.readDataToEndOfFile()
    let req: Request
    do { req = try decodeRequest(input) } catch {
        eprint("tgsort-ai: invalid request JSON: \(error)")
        return 2
    }

    let model = SystemLanguageModel.default
    if let reason = unavailableReason(model.availability) {
        let rs = req.files.map { Result(id: $0.id, choice: nil, error: "model unavailable: \(reason)") }
        out((try? encodeResponse(Response(results: rs))) ?? Data())
        return 0
    }

    let names = req.choices.map(\.name) + [unsureLabel]
    let instructions = buildInstructions(choices: req.choices)
    let schema: GenerationSchema
    do {
        schema = try GenerationSchema(root: DynamicGenerationSchema(name: "Category", anyOf: names), dependencies: [])
    } catch {
        let rs = req.files.map { Result(id: $0.id, choice: nil, error: "schema error: \(error)") }
        out((try? encodeResponse(Response(results: rs))) ?? Data())
        return 0
    }

    var results: [Result] = []
    var warmed = false
    for f in req.files {
        guard FileManager.default.fileExists(atPath: f.path) else {
            results.append(Result(id: f.id, choice: nil, error: "file not found: \(f.path)"))
            continue
        }
        let prompt = buildPrompt(name: f.name, content: extractContent(path: f.path))
        let session = LanguageModelSession(model: model, instructions: instructions)
        if !warmed { session.prewarm(); warmed = true }
        do {
            let r = try await session.respond(to: prompt, schema: schema)
            results.append(Result(id: f.id, choice: mapAnswer(r.content.jsonString, choices: req.choices)))
        } catch {
            results.append(Result(id: f.id, choice: nil, error: describe(error)))
        }
    }
    out((try? encodeResponse(Response(results: results))) ?? Data())
    return 0
}

let args = Array(CommandLine.arguments.dropFirst())
switch args.first {
case "--check":
    runCheck()
case "classify":
    exit(await runClassify())
case "--self-test":
    let fails = selfTest()
    if fails.isEmpty { print("self-test: ok") } else { fails.forEach { eprint("FAIL: \($0)") }; exit(1) }
default:
    eprint("usage: tgsort-ai --check | tgsort-ai classify  (JSON on stdin)")
    exit(64)
}
