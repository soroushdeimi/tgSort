import Foundation
import PDFKit

public let unsureLabel = "Unsure"
public let maxContentChars = 1500

public struct FileEntry: Codable, Sendable {
    public var id: String
    public var path: String
    public var name: String
}

public struct Choice: Codable, Sendable {
    public var name: String
    public var hint: String?
}

public struct Request: Codable, Sendable {
    public var files: [FileEntry]
    public var choices: [Choice]
}

public struct Result: Codable, Sendable, Equatable {
    public var id: String
    public var choice: String?
    public var error: String?

    public init(id: String, choice: String?, error: String? = nil) {
        self.id = id; self.choice = choice; self.error = error
    }

    // Always emit `choice` (null when absent); omit `error` when nil.
    enum CodingKeys: String, CodingKey { case id, choice, error }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(choice, forKey: .choice)
        try c.encodeIfPresent(error, forKey: .error)
    }
}

public struct Response: Codable, Sendable {
    public var results: [Result]
    public init(results: [Result]) { self.results = results }
}

/// Extensions treated as plain text.
public let textExtensions: Set<String> = [
    "txt", "md", "markdown", "json", "csv", "tsv", "html", "htm", "xml", "yaml", "yml",
    "toml", "ini", "conf", "cfg", "log", "rtf", "tex", "srt", "py", "js", "ts", "swift",
    "rs", "c", "h", "cpp", "java", "go", "sh", "sql", "css",
]

public func decodeRequest(_ data: Data) throws -> Request {
    try JSONDecoder().decode(Request.self, from: data)
}

public func encodeResponse(_ r: Response) throws -> Data {
    let enc = JSONEncoder()
    enc.outputFormatting = [.withoutEscapingSlashes, .sortedKeys]
    return try enc.encode(r)
}

/// Collapse whitespace runs and truncate to `limit` characters.
public func normalize(_ s: String, limit: Int = maxContentChars) -> String {
    let collapsed = s.split(whereSeparator: { $0.isWhitespace || $0.isNewline }).joined(separator: " ")
    return String(collapsed.prefix(limit))
}

/// Content excerpt for a file, or nil when only the name is usable.
public func extractContent(path: String) -> String? {
    let ext = (path as NSString).pathExtension.lowercased()
    if ext == "pdf" {
        guard let doc = PDFDocument(url: URL(fileURLWithPath: path)) else { return nil }
        var text = ""
        for i in 0..<min(2, doc.pageCount) {
            if let t = doc.page(at: i)?.string { text += t + " " }
            if text.count >= maxContentChars * 2 { break }
        }
        let n = normalize(text)
        return n.isEmpty ? nil : n
    }
    if textExtensions.contains(ext) {
        guard let fh = FileHandle(forReadingAtPath: path) else { return nil }
        defer { try? fh.close() }
        let data = (try? fh.read(upToCount: 8192)) ?? Data()
        let n = normalize(String(decoding: data, as: UTF8.self).replacingOccurrences(of: "\u{FFFD}", with: ""))
        return n.isEmpty ? nil : n
    }
    return nil
}

public func buildInstructions(choices: [Choice]) -> String {
    var s = "You sort downloaded files into folders. Folders:\n"
    for c in choices {
        if let h = c.hint, !h.isEmpty { s += "- \(c.name): \(h)\n" } else { s += "- \(c.name)\n" }
    }
    s += "- \(unsureLabel): none of the folders clearly fits\n"
    s += "Pick the single best folder from the file name and content. If none clearly fits, answer \(unsureLabel). The file name can be misleading; trust the content when present."
    return s
}

public func buildPrompt(name: String, content: String?) -> String {
    var p = "File name: \(String(name.prefix(200)))\n"
    if let c = content { p += "Content excerpt: \(c)\n" } else { p += "(No readable content; judge by name only.)\n" }
    p += "Which folder?"
    return p
}

/// Parse the model's JSON string output and map to a request choice.
public func mapAnswer(_ jsonString: String, choices: [Choice]) -> String? {
    var answer = jsonString
    if let d = jsonString.data(using: .utf8),
       let s = try? JSONDecoder().decode(String.self, from: d) { answer = s }
    if answer == unsureLabel { return nil }
    return choices.first(where: { $0.name == answer })?.name
}

/// Hidden self-test; returns failure messages.
public func selfTest() -> [String] {
    var fails: [String] = []
    func check(_ c: Bool, _ m: String) { if !c { fails.append(m) } }
    let choices = [Choice(name: "Papers", hint: "x"), Choice(name: "Books", hint: nil)]
    check(mapAnswer("\"Papers\"", choices: choices) == "Papers", "map Papers")
    check(mapAnswer("\"Unsure\"", choices: choices) == nil, "map Unsure")
    check(mapAnswer("\"Other\"", choices: choices) == nil, "map unknown")
    check(normalize("a \n\n b\t c") == "a b c", "normalize ws")
    check(normalize(String(repeating: "x", count: 5000)).count == maxContentChars, "truncate")
    check(buildInstructions(choices: choices).contains("- Books\n"), "no-hint line")
    check(buildInstructions(choices: choices).contains("- Papers: x\n"), "hint line")
    let req = try? decodeRequest(Data(#"{"files":[{"id":"0","path":"/a","name":"a"}],"choices":[{"name":"P"}]}"#.utf8))
    check(req?.files.count == 1 && req?.choices[0].hint == nil, "decode request")
    check((try? decodeRequest(Data("nope".utf8))) == nil, "bad json rejected")
    let out = try? encodeResponse(Response(results: [Result(id: "0", choice: nil, error: "e"), Result(id: "1", choice: "P")]))
    if let out,
       let obj = try? JSONSerialization.jsonObject(with: out) as? [String: Any],
       let rs = obj["results"] as? [[String: Any]], rs.count == 2 {
        check(rs[0]["id"] as? String == "0" && rs[0]["choice"] is NSNull && rs[0]["error"] as? String == "e", "encode null entry")
        check(rs[1]["id"] as? String == "1" && rs[1]["choice"] as? String == "P" && rs[1]["error"] == nil, "encode ok entry")
    } else {
        fails.append("encode shape")
    }
    check(extractContent(path: "/nonexistent.pdf") == nil, "missing pdf nil")
    return fails
}
