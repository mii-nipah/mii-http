import * as childProcess from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';

const languageId = 'mii-http';

interface CheckReport {
  diagnostics: CheckDiagnostic[];
}

interface CheckDiagnostic {
  kind: 'error' | 'warning';
  message: string;
  label: string;
  note?: string | null;
  span: {
    start: number;
    end: number;
  };
}

type SymbolSource = 'path' | 'query' | 'header' | 'var' | 'body' | 'bodyField';

interface SymbolInfo {
  name: string;
  source: SymbolSource;
  type: string;
  optional: boolean;
  range: vscode.Range;
  typeRange?: vscode.Range;
  detail?: string;
}

interface EndpointInfo {
  method: string;
  path: string;
  line: number;
}

interface EndpointSymbols {
  pathParams: SymbolInfo[];
  queryParams: SymbolInfo[];
  headers: SymbolInfo[];
  vars: SymbolInfo[];
  bodyFields: SymbolInfo[];
  hasBody: boolean;
  body?: SymbolInfo;
}

interface CommandSpec {
  command: string;
  args: string[];
}

export function activate(context: vscode.ExtensionContext): void {
  const diagnostics = vscode.languages.createDiagnosticCollection('mii-http');
  const checker = new Checker(diagnostics);

  context.subscriptions.push(
    diagnostics,
    checker,
    vscode.workspace.onDidOpenTextDocument(document => checker.queue(document)),
    vscode.workspace.onDidSaveTextDocument(document => checker.queue(document, 0)),
    vscode.workspace.onDidCloseTextDocument(document => diagnostics.delete(document.uri)),
    vscode.workspace.onDidChangeTextDocument(event => {
      if (checkOnChange(event.document)) {
        checker.queue(event.document);
      }
    }),
    vscode.workspace.onDidChangeConfiguration(event => {
      if (event.affectsConfiguration('miiHttp')) {
        for (const document of vscode.workspace.textDocuments) {
          checker.queue(document, 0);
        }
      }
    }),
    vscode.languages.registerCompletionItemProvider(
      { language: languageId },
      new CompletionProvider(),
      '%',
      ':',
      '^',
      '@',
      '$',
      '[',
      '{',
      ' '
    ),
    vscode.languages.registerHoverProvider({ language: languageId }, new MiiHoverProvider()),
    vscode.languages.registerDefinitionProvider({ language: languageId }, new MiiDefinitionProvider())
  );

  for (const document of vscode.workspace.textDocuments) {
    checker.queue(document, 0);
  }
}

export function deactivate(): void {}

class Checker implements vscode.Disposable {
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();

  constructor(private readonly diagnostics: vscode.DiagnosticCollection) {}

  dispose(): void {
    for (const timer of this.timers.values()) {
      clearTimeout(timer);
    }
    this.timers.clear();
  }

  queue(document: vscode.TextDocument, delay = 250): void {
    if (!isMiiHttp(document)) {
      return;
    }
    const key = document.uri.toString();
    const previous = this.timers.get(key);
    if (previous) {
      clearTimeout(previous);
    }
    this.timers.set(
      key,
      setTimeout(() => {
        this.timers.delete(key);
        void this.validate(document);
      }, delay)
    );
  }

  private async validate(document: vscode.TextDocument): Promise<void> {
    const version = document.version;
    const text = document.getText();
    let tempDir: string | undefined;

    try {
      tempDir = await fs.promises.mkdtemp(path.join(os.tmpdir(), 'mii-http-'));
      const tempFile = path.join(tempDir, tempFileName(document));
      await fs.promises.writeFile(tempFile, text, 'utf8');

      const command = resolveCommand(document);
      const checkArgs = configuration(document).get<string[]>('checkArgs', ['--check', '--json']);
      const result = await run(command.command, [...command.args, ...checkArgs, tempFile], cwd(document));
      const report = parseReport(result.stdout, result.stderr);

      if (document.version !== version) {
        return;
      }
      this.diagnostics.set(
        document.uri,
        report.diagnostics.map(diagnostic => toVsCodeDiagnostic(document, diagnostic))
      );
    } catch (error) {
      if (document.version !== version) {
        return;
      }
      this.diagnostics.set(document.uri, [checkerFailure(document, error)]);
    } finally {
      if (tempDir) {
        await fs.promises.rm(tempDir, { recursive: true, force: true });
      }
    }
  }
}

class CompletionProvider implements vscode.CompletionItemProvider {
  provideCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position
  ): vscode.ProviderResult<vscode.CompletionItem[]> {
    const line = document.lineAt(position.line).text;
    const before = line.slice(0, position.character);

    if (/^\s*Exec:\s*/.test(line)) {
      return execCompletions(document, position);
    }
    if (/\bBODY\s+\w*$/.test(before)) {
      return bodyCompletions();
    }
    if (isTypePosition(before)) {
      return typeCompletions();
    }
    if (/^\s*$/.test(before)) {
      return [...methodCompletions(), ...setupCompletions(), ...directiveCompletions()];
    }
    if (currentEndpoint(document, position.line)) {
      return directiveCompletions();
    }
    return [...methodCompletions(), ...setupCompletions()];
  }
}

class MiiHoverProvider implements vscode.HoverProvider {
  provideHover(document: vscode.TextDocument, position: vscode.Position): vscode.ProviderResult<vscode.Hover> {
    const symbol = symbolAt(document, position) ?? symbolForReferenceAt(document, position)?.symbol;
    if (symbol) {
      return new vscode.Hover(symbolMarkdown(symbol));
    }

    const keyword = keywordAt(document, position);
    if (keyword) {
      return new vscode.Hover(keywordMarkdown(keyword));
    }

    return undefined;
  }
}

class MiiDefinitionProvider implements vscode.DefinitionProvider {
  provideDefinition(
    document: vscode.TextDocument,
    position: vscode.Position
  ): vscode.ProviderResult<vscode.Definition> {
    const resolved = symbolForReferenceAt(document, position) ?? symbolAt(document, position);
    if (!resolved) {
      return undefined;
    }
    const symbol = 'symbol' in resolved ? resolved.symbol : resolved;
    return new vscode.Location(document.uri, symbol.range);
  }
}

function isMiiHttp(document: vscode.TextDocument): boolean {
  return document.languageId === languageId;
}

function configuration(document: vscode.TextDocument): vscode.WorkspaceConfiguration {
  return vscode.workspace.getConfiguration('miiHttp', document.uri);
}

function checkOnChange(document: vscode.TextDocument): boolean {
  return isMiiHttp(document) && configuration(document).get<boolean>('checkOnChange', true);
}

function resolveCommand(document: vscode.TextDocument): CommandSpec {
  const configured = configuration(document).get<string>('executable', 'mii-http');
  if (configured !== 'mii-http') {
    return { command: configured, args: [] };
  }

  const local = findWorkspaceBinary(document);
  if (local) {
    return { command: local, args: [] };
  }

  return { command: configured, args: [] };
}

function findWorkspaceBinary(document: vscode.TextDocument): string | undefined {
  const folder = vscode.workspace.getWorkspaceFolder(document.uri);
  const start = folder?.uri.fsPath ?? (document.uri.scheme === 'file' ? path.dirname(document.uri.fsPath) : undefined);
  if (!start) {
    return undefined;
  }

  const binary = process.platform === 'win32' ? 'mii-http.exe' : 'mii-http';
  let current = start;
  while (true) {
    const candidate = path.join(current, 'target', 'debug', binary);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      return undefined;
    }
    current = parent;
  }
}

function cwd(document: vscode.TextDocument): string | undefined {
  const folder = vscode.workspace.getWorkspaceFolder(document.uri);
  if (folder) {
    return folder.uri.fsPath;
  }
  if (document.uri.scheme === 'file') {
    return path.dirname(document.uri.fsPath);
  }
  return undefined;
}

function tempFileName(document: vscode.TextDocument): string {
  if (document.uri.scheme === 'file') {
    return path.basename(document.uri.fsPath);
  }
  return 'untitled.http';
}

function run(command: string, args: string[], runCwd: string | undefined): Promise<{ stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = childProcess.spawn(command, args, {
      cwd: runCwd,
      windowsHide: true
    });
    let stdout = '';
    let stderr = '';
    const timeout = setTimeout(() => {
      child.kill();
      reject(new Error('mii-http check timed out'));
    }, 10000);

    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      stdout += chunk;
    });
    child.stderr.on('data', chunk => {
      stderr += chunk;
    });
    child.on('error', error => {
      clearTimeout(timeout);
      reject(error);
    });
    child.on('close', () => {
      clearTimeout(timeout);
      resolve({ stdout, stderr });
    });
  });
}

function parseReport(stdout: string, stderr: string): CheckReport {
  const start = stdout.indexOf('{');
  if (start < 0) {
    const detail = stderr.trim() || stdout.trim();
    const suffix = detail ? `: ${truncate(detail)}` : '';
    throw new Error(`mii-http did not print JSON diagnostics${suffix}`);
  }
  const parsed: unknown = JSON.parse(stdout.slice(start));
  if (!isReport(parsed)) {
    throw new Error('mii-http printed an unexpected diagnostics shape');
  }
  return parsed;
}

function truncate(value: string): string {
  return value.length <= 300 ? value : `${value.slice(0, 300)}...`;
}

function isReport(value: unknown): value is CheckReport {
  if (!value || typeof value !== 'object') {
    return false;
  }
  const maybe = value as { diagnostics?: unknown };
  return Array.isArray(maybe.diagnostics);
}

function toVsCodeDiagnostic(document: vscode.TextDocument, diagnostic: CheckDiagnostic): vscode.Diagnostic {
  const range = rangeFromSpan(document, diagnostic.span.start, diagnostic.span.end);
  const message = [diagnostic.message, diagnostic.label, diagnostic.note]
    .filter((part): part is string => typeof part === 'string' && part.length > 0)
    .filter((part, index, parts) => parts.indexOf(part) === index)
    .join('\n');
  const item = new vscode.Diagnostic(
    range,
    message,
    diagnostic.kind === 'error' ? vscode.DiagnosticSeverity.Error : vscode.DiagnosticSeverity.Warning
  );
  item.source = 'mii-http';
  return item;
}

function checkerFailure(document: vscode.TextDocument, error: unknown): vscode.Diagnostic {
  const firstLine = document.lineAt(0);
  const range = firstLine.range.isEmpty
    ? new vscode.Range(new vscode.Position(0, 0), new vscode.Position(0, 0))
    : new vscode.Range(firstLine.range.start, firstLine.range.start.translate(0, 1));
  const message = error instanceof Error ? error.message : String(error);
  const diagnostic = new vscode.Diagnostic(
    range,
    `mii-http check failed: ${message}`,
    vscode.DiagnosticSeverity.Error
  );
  diagnostic.source = 'mii-http';
  return diagnostic;
}

function rangeFromSpan(document: vscode.TextDocument, startByte: number, endByte: number): vscode.Range {
  const start = positionAtByteOffset(document, startByte);
  let end = positionAtByteOffset(document, Math.max(startByte, endByte));
  if (end.isEqual(start)) {
    const line = document.lineAt(start.line);
    if (start.character < line.text.length) {
      end = start.translate(0, 1);
    }
  }
  return new vscode.Range(start, end);
}

function positionAtByteOffset(document: vscode.TextDocument, target: number): vscode.Position {
  const text = document.getText();
  let byte = 0;
  let line = 0;
  let character = 0;

  for (const ch of text) {
    const size = Buffer.byteLength(ch, 'utf8');
    if (byte + size > target) {
      break;
    }
    byte += size;
    if (ch === '\n') {
      line += 1;
      character = 0;
    } else if (ch !== '\r') {
      character += ch.length;
    }
  }

  return new vscode.Position(line, character);
}

function methodCompletions(): vscode.CompletionItem[] {
  return ['GET', 'POST', 'PUT', 'DELETE', 'PATCH'].map(method => {
    const item = new vscode.CompletionItem(method, vscode.CompletionItemKind.Keyword);
    item.insertText = new vscode.SnippetString(`${method} /\${1:path}\nResponse-Type text/plain\nExec: \${2:echo ok}`);
    item.detail = 'mii-http endpoint';
    return item;
  });
}

function setupCompletions(): vscode.CompletionItem[] {
  return [
    snippet('VERSION', 'VERSION ${1:1}', 'setup directive'),
    snippet('BASE', 'BASE /${1:api}', 'setup directive'),
    snippet('AUTH Bearer', 'AUTH Bearer [HEADER ${1:Authorization}]', 'setup directive'),
    snippet('JWT_VERIFIER', 'JWT_VERIFIER [ENV ${1:JWT_SECRET}]', 'setup directive'),
    snippet('TOKEN_SECRET', 'TOKEN_SECRET [ENV ${1:TOKEN_SECRET}]', 'setup directive'),
    snippet('MAX_BODY_SIZE', 'MAX_BODY_SIZE ${1:1mb}', 'setup directive'),
    snippet('MAX_QUERY_PARAM_SIZE', 'MAX_QUERY_PARAM_SIZE ${1:100}', 'setup directive'),
    snippet('MAX_HEADER_SIZE', 'MAX_HEADER_SIZE ${1:200}', 'setup directive'),
    snippet('TIMEOUT', 'TIMEOUT ${1:30s}', 'setup directive')
  ];
}

function directiveCompletions(): vscode.CompletionItem[] {
  return [
    snippet('Response-Type', 'Response-Type ${1:text/plain}', 'endpoint directive'),
    snippet('QUERY', 'QUERY ${1:name}: ${2:/[a-zA-Z0-9_]+/}', 'endpoint directive'),
    snippet('HEADER', 'HEADER ${1:X-Header}: ${2:/[a-zA-Z0-9_]+/}', 'endpoint directive'),
    snippet('VAR', 'VAR ${1:name} [ENV ${2:NAME}]', 'endpoint directive'),
    snippet('BODY json', 'BODY json {\n  ${1:field}: ${2:int}\n}', 'endpoint directive'),
    snippet('BODY form', 'BODY form {\n  ${1:field}: ${2:/[a-zA-Z0-9_]+/}\n}', 'endpoint directive'),
    snippet('BODY string', 'BODY string', 'endpoint directive'),
    snippet('BODY binary', 'BODY binary', 'endpoint directive'),
    snippet('Exec', 'Exec: ${1:echo ok}', 'endpoint directive')
  ];
}

function bodyCompletions(): vscode.CompletionItem[] {
  return [
    snippet('json', 'json {\n  ${1:field}: ${2:int}\n}', 'body kind'),
    snippet('form', 'form {\n  ${1:field}: ${2:/[a-zA-Z0-9_]+/}\n}', 'body kind'),
    keyword('string', 'body kind'),
    keyword('binary', 'body kind')
  ];
}

function typeCompletions(): vscode.CompletionItem[] {
  return [
    keyword('int', 'type'),
    keyword('float', 'type'),
    keyword('boolean', 'type'),
    keyword('uuid', 'type'),
    keyword('string', 'stdin-only body type'),
    keyword('json', 'stdin-only body type'),
    keyword('binary', 'body type'),
    snippet('int range', 'int(${1:0}..${2:100})', 'type'),
    snippet('float range', 'float(${1:0.0}..${2:1.0})', 'type'),
    snippet('regex', '/${1:[a-zA-Z0-9_]+}/', 'type'),
    snippet('union', '${1:on}|${2:off}|${3:auto}', 'type')
  ];
}

function execCompletions(document: vscode.TextDocument, position: vscode.Position): vscode.CompletionItem[] {
  const symbols = collectSymbols(document, position.line);
  const range = referencePrefixRange(document, position);
  const completions: vscode.CompletionItem[] = [];

  for (const symbol of symbols.queryParams) {
    completions.push(reference(`%${symbol.name}`, symbolCompletionDetail(symbol), range));
  }
  for (const symbol of symbols.pathParams) {
    completions.push(reference(`:${symbol.name}`, symbolCompletionDetail(symbol), range));
  }
  for (const symbol of symbols.headers) {
    completions.push(reference(`^${symbol.name}`, symbolCompletionDetail(symbol), range));
  }
  for (const symbol of symbols.vars) {
    completions.push(reference(`@${symbol.name}`, symbolCompletionDetail(symbol), range));
  }
  if (symbols.body) {
    completions.push(reference('$', symbolCompletionDetail(symbols.body), range));
  }
  for (const symbol of symbols.bodyFields) {
    completions.push(reference(`$.${symbol.name}`, symbolCompletionDetail(symbol), range));
  }

  completions.push(snippet('$ | command', '$ | ${1:command}', 'pipe body to stdin'));
  return completions;
}

interface ReferenceInfo {
  source: SymbolSource;
  name: string;
  range: vscode.Range;
}

interface ReferenceResolution {
  reference: ReferenceInfo;
  symbol: SymbolInfo;
}

function collectSymbols(document: vscode.TextDocument, line: number): EndpointSymbols {
  const endpoint = currentEndpoint(document, line);
  const symbols: EndpointSymbols = {
    pathParams: [],
    queryParams: [],
    headers: [],
    vars: [],
    bodyFields: [],
    hasBody: false
  };
  if (!endpoint) {
    return symbols;
  }

  symbols.pathParams = pathParams(document, endpoint);
  let bodyBlock = false;
  const endLine = endpointEndLine(document, endpoint.line);
  for (let i = endpoint.line + 1; i <= endLine; i += 1) {
    const raw = document.lineAt(i).text;
    const text = raw.trim();
    if (text === '' || text.startsWith('#')) {
      continue;
    }
    if (bodyBlock) {
      if (text === '}') {
        bodyBlock = false;
        continue;
      }
      const field = raw.match(/^(\s*)([A-Za-z_][A-Za-z0-9_-]*)(\?)?\s*:\s*(.+?)\s*,?\s*$/);
      if (field) {
        const name = field[2];
        const type = field[4].trim();
        symbols.bodyFields.push({
          name,
          source: 'bodyField',
          type,
          optional: Boolean(field[3]),
          range: rangeForSubstring(i, raw, name),
          typeRange: rangeForSubstring(i, raw, type, raw.indexOf(':') + 1)
        });
      }
      continue;
    }
    const body = raw.match(/^\s*BODY\s+(json|form|string|binary)\b(.*)$/);
    if (body) {
      symbols.hasBody = true;
      const kind = body[1];
      const bodyRange = rangeForSubstring(i, raw, kind);
      symbols.body = {
        name: '$',
        source: 'body',
        type: kind,
        optional: false,
        range: bodyRange,
        typeRange: bodyRange,
        detail: 'Whole request body.'
      };
      bodyBlock = body[2].includes('{');
      continue;
    }
    const query = raw.match(/^\s*QUERY\s+([A-Za-z_][A-Za-z0-9_-]*)(\?)?\s*:\s*(.+)$/);
    if (query) {
      symbols.queryParams.push(fieldSymbol(i, raw, query[1], query[2], query[3], 'query'));
      continue;
    }
    const header = raw.match(/^\s*HEADER\s+([A-Za-z_][A-Za-z0-9_-]*)(\?)?\s*:\s*(.+)$/);
    if (header) {
      symbols.headers.push(fieldSymbol(i, raw, header[1], header[2], header[3], 'header'));
      continue;
    }
    const variable = raw.match(/^\s*VAR\s+([A-Za-z_][A-Za-z0-9_-]*)\s+(.+)$/);
    if (variable) {
      const name = variable[1];
      const source = variable[2].trim();
      symbols.vars.push({
        name,
        source: 'var',
        type: 'value source',
        optional: false,
        range: rangeForSubstring(i, raw, name),
        typeRange: rangeForSubstring(i, raw, source),
        detail: `Loaded from ${source}.`
      });
    }
  }
  return symbols;
}

function fieldSymbol(
  line: number,
  raw: string,
  name: string,
  optionalMarker: string | undefined,
  typeSource: string,
  source: 'query' | 'header'
): SymbolInfo {
  const type = cleanTypeSource(typeSource);
  return {
    name,
    source,
    type,
    optional: Boolean(optionalMarker),
    range: rangeForSubstring(line, raw, name),
    typeRange: rangeForSubstring(line, raw, type, raw.indexOf(':') + 1)
  };
}

function cleanTypeSource(typeSource: string): string {
  return typeSource.trim().replace(/\s+#.*$/, '').replace(/,$/, '').trim();
}

function currentEndpoint(document: vscode.TextDocument, line: number): EndpointInfo | undefined {
  let endpoint: EndpointInfo | undefined;
  for (let i = 0; i <= line; i += 1) {
    const text = document.lineAt(i).text;
    const match = text.match(/^\s*(GET|POST|PUT|DELETE|PATCH)\s+(\S+)/);
    if (match) {
      endpoint = { method: match[1], line: i, path: match[2] };
    }
  }
  return endpoint;
}

function endpointEndLine(document: vscode.TextDocument, endpointLine: number): number {
  for (let i = endpointLine + 1; i < document.lineCount; i += 1) {
    if (/^\s*(GET|POST|PUT|DELETE|PATCH)\s+\S+/.test(document.lineAt(i).text)) {
      return i - 1;
    }
  }
  return document.lineCount - 1;
}

function pathParams(document: vscode.TextDocument, endpoint: EndpointInfo): SymbolInfo[] {
  const params: SymbolInfo[] = [];
  const lineText = document.lineAt(endpoint.line).text;
  const pathStart = lineText.indexOf(endpoint.path);
  const matcher = /:([A-Za-z_][A-Za-z0-9_-]*)(?::([^/\s]+))?/g;
  for (const match of endpoint.path.matchAll(matcher)) {
    const index = match.index ?? 0;
    const name = match[1];
    const type = match[2] ?? 'string';
    const nameStart = pathStart + index + 1;
    const typeStart = match[2] ? nameStart + name.length + 1 : undefined;
    const range = new vscode.Range(endpoint.line, nameStart, endpoint.line, nameStart + name.length);
    const typeRange =
      typeStart === undefined ? undefined : new vscode.Range(endpoint.line, typeStart, endpoint.line, typeStart + type.length);
    params.push({
      name,
      source: 'path',
      type,
      optional: false,
      range,
      typeRange
    });
  }
  return params;
}

function allSymbols(symbols: EndpointSymbols): SymbolInfo[] {
  return [
    ...symbols.pathParams,
    ...symbols.queryParams,
    ...symbols.headers,
    ...symbols.vars,
    ...symbols.bodyFields,
    ...(symbols.body ? [symbols.body] : [])
  ];
}

function symbolAt(document: vscode.TextDocument, position: vscode.Position): SymbolInfo | undefined {
  const symbols = collectSymbols(document, position.line);
  return allSymbols(symbols).find(symbol => containsPosition(symbol.range, position) || containsPosition(symbol.typeRange, position));
}

function symbolForReferenceAt(document: vscode.TextDocument, position: vscode.Position): ReferenceResolution | undefined {
  const reference = referenceAt(document, position);
  if (!reference) {
    return undefined;
  }

  const symbols = collectSymbols(document, position.line);
  const symbol = resolveReference(symbols, reference);
  return symbol ? { reference, symbol } : undefined;
}

function resolveReference(symbols: EndpointSymbols, reference: ReferenceInfo): SymbolInfo | undefined {
  switch (reference.source) {
    case 'query':
      return symbols.queryParams.find(symbol => symbol.name === reference.name);
    case 'path':
      return symbols.pathParams.find(symbol => symbol.name === reference.name);
    case 'header':
      return symbols.headers.find(symbol => symbol.name === reference.name);
    case 'var':
      return symbols.vars.find(symbol => symbol.name === reference.name);
    case 'body':
      return symbols.body;
    case 'bodyField':
      return symbols.bodyFields.find(symbol => symbol.name === reference.name);
  }
}

function referenceAt(document: vscode.TextDocument, position: vscode.Position): ReferenceInfo | undefined {
  const line = document.lineAt(position.line).text;
  const matcher = /(%[A-Za-z_][A-Za-z0-9_-]*|:[A-Za-z_][A-Za-z0-9_-]*|\^[A-Za-z_][A-Za-z0-9_-]*|@[A-Za-z_][A-Za-z0-9_-]*|\$(?:\.[A-Za-z_][A-Za-z0-9_-]*)*)/g;
  for (const match of line.matchAll(matcher)) {
    const index = match.index ?? 0;
    const value = match[0];
    const range = new vscode.Range(position.line, index, position.line, index + value.length);
    if (!containsPosition(range, position)) {
      continue;
    }
    if (value.startsWith('%')) {
      return { source: 'query', name: value.slice(1), range };
    }
    if (value.startsWith(':')) {
      return { source: 'path', name: value.slice(1), range };
    }
    if (value.startsWith('^')) {
      return { source: 'header', name: value.slice(1), range };
    }
    if (value.startsWith('@')) {
      return { source: 'var', name: value.slice(1), range };
    }
    if (value === '$') {
      return { source: 'body', name: '$', range };
    }
    if (value.startsWith('$.')) {
      return { source: 'bodyField', name: value.slice(2).split('.')[0], range };
    }
  }
  return undefined;
}

function containsPosition(range: vscode.Range | undefined, position: vscode.Position): boolean {
  if (!range) {
    return false;
  }
  return range.contains(position) || range.end.isEqual(position);
}

function rangeForSubstring(line: number, raw: string, value: string, from = 0): vscode.Range {
  const start = Math.max(0, raw.indexOf(value, from));
  return new vscode.Range(line, start, line, start + value.length);
}

function symbolCompletionDetail(symbol: SymbolInfo): string {
  const optional = symbol.optional ? 'optional ' : '';
  return `${optional}${sourceLabel(symbol.source)}: ${symbol.type}`;
}

function sourceLabel(source: SymbolSource): string {
  switch (source) {
    case 'path':
      return 'path parameter';
    case 'query':
      return 'query parameter';
    case 'header':
      return 'header';
    case 'var':
      return 'var';
    case 'body':
      return 'body';
    case 'bodyField':
      return 'body field';
  }
}

function keywordAt(document: vscode.TextDocument, position: vscode.Position): string | undefined {
  const line = document.lineAt(position.line).text;
  const keywords = Object.keys(keywordDescriptions).sort((a, b) => b.length - a.length);
  for (const keyword of keywords) {
    const matcher = new RegExp(`(^|[^A-Za-z0-9_-])(${escapeRegExp(keyword)})(?=$|[^A-Za-z0-9_-])`, 'g');
    for (const match of line.matchAll(matcher)) {
      const start = (match.index ?? 0) + match[1].length;
      const range = new vscode.Range(position.line, start, position.line, start + keyword.length);
      if (containsPosition(range, position)) {
        return keyword;
      }
    }
  }
  return undefined;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function symbolMarkdown(symbol: SymbolInfo): vscode.MarkdownString {
  const markdown = new vscode.MarkdownString();
  const name = symbol.source === 'body' ? '$' : symbol.name;
  markdown.appendMarkdown(`**${sourceLabel(symbol.source)} \`${name}\`**\n\n`);
  markdown.appendMarkdown(`Type: \`${symbol.type}\`\n\n`);
  if (symbol.optional) {
    markdown.appendMarkdown('Optional: yes\n\n');
  }
  const examples = examplesForType(symbol.type);
  if (examples.length > 0) {
    markdown.appendMarkdown(`Examples: ${examples.map(example => `\`${example}\``).join(', ')}\n\n`);
  }
  if (symbol.detail) {
    markdown.appendMarkdown(`${symbol.detail}\n\n`);
  }
  markdown.appendMarkdown(`Declared at line ${symbol.range.start.line + 1}.`);
  return markdown;
}

function keywordMarkdown(keyword: string): vscode.MarkdownString {
  const doc = keywordDescriptions[keyword];
  const markdown = new vscode.MarkdownString();
  markdown.appendMarkdown(`**${keyword}**\n\n${doc.description}`);
  if (doc.syntax) {
    markdown.appendMarkdown(`\n\nSyntax: \`${doc.syntax}\``);
  }
  return markdown;
}

function examplesForType(type: string): string[] {
  const normalized = type.trim();
  const arrayInner = normalized.match(/^\[(.+)\]$/);
  if (arrayInner) {
    const inner = examplesForType(arrayInner[1])[0] ?? 'value';
    return [`[${inner}]`];
  }

  const intRange = normalized.match(/^int\((-?\d+)\.\.(-?\d+)\)$/);
  if (intRange) {
    const min = Number(intRange[1]);
    const max = Number(intRange[2]);
    const mid = Math.trunc((min + max) / 2);
    return uniqueStrings([String(min), String(mid), String(max)]);
  }

  const floatRange = normalized.match(/^float\((-?\d+(?:\.\d+)?)\.\.(-?\d+(?:\.\d+)?)\)$/);
  if (floatRange) {
    return uniqueStrings([floatRange[1], midpoint(floatRange[1], floatRange[2]), floatRange[2]]);
  }

  if (normalized.startsWith('/') && normalized.endsWith('/')) {
    return regexExamples(normalized.slice(1, -1));
  }

  if (normalized.includes('|')) {
    return normalized.split('|').map(part => part.trim()).filter(Boolean).slice(0, 4);
  }

  switch (normalized) {
    case 'int':
      return ['0', '42', '-7'];
    case 'float':
      return ['0.5', '3.14'];
    case 'boolean':
    case 'bool':
      return ['true', 'false'];
    case 'uuid':
      return ['550e8400-e29b-41d4-a716-446655440000'];
    case 'string':
      return ['hello world'];
    case 'json':
      return ['{"ok": true}'];
    case 'binary':
      return ['<raw bytes>'];
    case 'form':
      return ['name=mii&age=42'];
    default:
      return [];
  }
}

function midpoint(a: string, b: string): string {
  const mid = (Number(a) + Number(b)) / 2;
  return Number.isInteger(mid) ? `${mid}.0` : String(mid);
}

function regexExamples(pattern: string): string[] {
  if (/[0-9]/.test(pattern) && !/[a-zA-Z]/.test(pattern)) {
    return ['42'];
  }
  if (pattern.includes('a-z') || pattern.includes('A-Z')) {
    if (pattern.includes('0-9') || pattern.includes('_')) {
      return ['mii_123'];
    }
    if (pattern.includes(' ')) {
      return ['hello world'];
    }
    return ['mii'];
  }
  if (pattern === '.*' || pattern === '.+') {
    return ['anything'];
  }
  return [`matches /${pattern}/`];
}

function uniqueStrings(values: string[]): string[] {
  return [...new Set(values)];
}

interface KeywordDescription {
  description: string;
  syntax?: string;
}

const keywordDescriptions: Record<string, KeywordDescription> = {
  VERSION: {
    description: 'Declares the spec format version. Today this is normally `1`.',
    syntax: 'VERSION 1'
  },
  BASE: {
    description: 'Sets a path prefix for every endpoint in the file.',
    syntax: 'BASE /api'
  },
  AUTH: {
    description: 'Configures authentication for every endpoint. Bearer tokens are read from the configured header.',
    syntax: 'AUTH Bearer [HEADER Authorization]'
  },
  Bearer: {
    description: 'Bearer-token authentication scheme used by `AUTH`.',
    syntax: 'AUTH Bearer [HEADER Authorization]'
  },
  JWT_VERIFIER: {
    description: 'Declares the command or value source used to validate JWT bearer tokens.',
    syntax: 'JWT_VERIFIER [ENV JWT_SECRET]'
  },
  TOKEN_SECRET: {
    description: 'Declares a shared secret used to validate bearer tokens.',
    syntax: 'TOKEN_SECRET [ENV TOKEN_SECRET]'
  },
  MAX_BODY_SIZE: {
    description: 'Limits the accepted request body size before the endpoint command can run.',
    syntax: 'MAX_BODY_SIZE 1mb'
  },
  MAX_QUERY_PARAM_SIZE: {
    description: 'Limits the size of each query parameter.',
    syntax: 'MAX_QUERY_PARAM_SIZE 100'
  },
  MAX_HEADER_SIZE: {
    description: 'Limits the size of each request header value.',
    syntax: 'MAX_HEADER_SIZE 200'
  },
  TIMEOUT: {
    description: 'Sets the maximum execution time for endpoint commands.',
    syntax: 'TIMEOUT 30s'
  },
  GET: {
    description: 'Declares a GET endpoint. Path parameters use `:name:type` inside the path.',
    syntax: 'GET /users/:user_id:uuid'
  },
  POST: {
    description: 'Declares a POST endpoint. Usually paired with a `BODY` directive.',
    syntax: 'POST /submit'
  },
  PUT: {
    description: 'Declares a PUT endpoint.',
    syntax: 'PUT /users/:user_id:uuid'
  },
  DELETE: {
    description: 'Declares a DELETE endpoint.',
    syntax: 'DELETE /users/:user_id:uuid'
  },
  PATCH: {
    description: 'Declares a PATCH endpoint.',
    syntax: 'PATCH /users/:user_id:uuid'
  },
  'Response-Type': {
    description: 'Sets the response content type emitted by this endpoint.',
    syntax: 'Response-Type application/json'
  },
  QUERY: {
    description: 'Declares a query parameter and the type used to validate it before command execution.',
    syntax: 'QUERY name?: /[a-zA-Z0-9_]+/'
  },
  HEADER: {
    description: 'Declares a request header and the type used to validate it.',
    syntax: 'HEADER X-Request-Id: uuid'
  },
  VAR: {
    description: 'Declares a named value loaded from an environment variable, header, or literal.',
    syntax: 'VAR greeting [ENV GREETING]'
  },
  BODY: {
    description: 'Declares the request body shape. Schematized `json` and `form` bodies expose fields to `Exec`.',
    syntax: 'BODY json { ... }'
  },
  Exec: {
    description: 'Declares the command pipeline to run for this endpoint. Values are passed as argv or stdin, not shell-spliced.',
    syntax: 'Exec: echo [%name]'
  },
  ENV: {
    description: 'Reads a value from an environment variable.',
    syntax: '[ENV TOKEN_SECRET]'
  }
};

function referencePrefixRange(document: vscode.TextDocument, position: vscode.Position): vscode.Range | undefined {
  const before = document.lineAt(position.line).text.slice(0, position.character);
  const match = before.match(/(?:[%:^@][A-Za-z0-9_-]*|\$(?:\.[A-Za-z0-9_-]*)*)$/);
  if (!match) {
    return undefined;
  }
  return new vscode.Range(position.translate(0, -match[0].length), position);
}

function isTypePosition(before: string): boolean {
  return /(?:QUERY|HEADER)\s+[A-Za-z_][A-Za-z0-9_-]*\??\s*:\s*\S*$/.test(before)
    || /^\s*[A-Za-z_][A-Za-z0-9_-]*\??\s*:\s*\S*$/.test(before)
    || /\bBODY\s+(?:json|form)\s*\{[^}]*$/.test(before);
}

function keyword(label: string, detail: string): vscode.CompletionItem {
  const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Keyword);
  item.detail = detail;
  return item;
}

function snippet(label: string, insertText: string, detail: string): vscode.CompletionItem {
  const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Snippet);
  item.insertText = new vscode.SnippetString(insertText);
  item.detail = detail;
  return item;
}

function reference(label: string, detail: string, range: vscode.Range | undefined): vscode.CompletionItem {
  const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Variable);
  item.insertText = label;
  item.detail = detail;
  item.range = range;
  return item;
}
