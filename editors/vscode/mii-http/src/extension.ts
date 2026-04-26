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

interface EndpointSymbols {
  pathParams: string[];
  queryParams: string[];
  headers: string[];
  vars: string[];
  bodyFields: string[];
  hasBody: boolean;
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
    )
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

  for (const name of symbols.queryParams) {
    completions.push(reference(`%${name}`, 'query parameter', range));
  }
  for (const name of symbols.pathParams) {
    completions.push(reference(`:${name}`, 'path parameter', range));
  }
  for (const name of symbols.headers) {
    completions.push(reference(`^${name}`, 'header', range));
  }
  for (const name of symbols.vars) {
    completions.push(reference(`@${name}`, 'var', range));
  }
  if (symbols.hasBody) {
    completions.push(reference('$', 'whole body', range));
  }
  for (const name of symbols.bodyFields) {
    completions.push(reference(`$.${name}`, 'body field', range));
  }

  completions.push(snippet('$ | command', '$ | ${1:command}', 'pipe body to stdin'));
  return completions;
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

  symbols.pathParams = pathParams(endpoint.path);
  let bodyBlock = false;
  for (let i = endpoint.line + 1; i < line; i += 1) {
    const text = document.lineAt(i).text.trim();
    if (text === '' || text.startsWith('#')) {
      continue;
    }
    if (bodyBlock) {
      if (text === '}') {
        bodyBlock = false;
        continue;
      }
      const field = text.match(/^([A-Za-z_][A-Za-z0-9_-]*)\??\s*:/);
      if (field) {
        symbols.bodyFields.push(field[1]);
      }
      continue;
    }
    const body = text.match(/^BODY\s+(json|form|string|binary)\b(.*)$/);
    if (body) {
      symbols.hasBody = true;
      bodyBlock = body[2].includes('{');
      continue;
    }
    const query = text.match(/^QUERY\s+([A-Za-z_][A-Za-z0-9_-]*)\??\s*:/);
    if (query) {
      symbols.queryParams.push(query[1]);
      continue;
    }
    const header = text.match(/^HEADER\s+([A-Za-z_][A-Za-z0-9_-]*)\??\s*:/);
    if (header) {
      symbols.headers.push(header[1]);
      continue;
    }
    const variable = text.match(/^VAR\s+([A-Za-z_][A-Za-z0-9_-]*)\b/);
    if (variable) {
      symbols.vars.push(variable[1]);
    }
  }
  return symbols;
}

function currentEndpoint(document: vscode.TextDocument, line: number): { line: number; path: string } | undefined {
  let endpoint: { line: number; path: string } | undefined;
  for (let i = 0; i <= line; i += 1) {
    const text = document.lineAt(i).text;
    const match = text.match(/^\s*(GET|POST|PUT|DELETE|PATCH)\s+(\S+)/);
    if (match) {
      endpoint = { line: i, path: match[2] };
    }
  }
  return endpoint;
}

function pathParams(endpointPath: string): string[] {
  const params: string[] = [];
  for (const segment of endpointPath.split('/')) {
    const match = segment.match(/^:([A-Za-z_][A-Za-z0-9_-]*)/);
    if (match) {
      params.push(match[1]);
    }
  }
  return params;
}

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
