import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';

import '../net/link.dart';
import '../net/rpc.dart';

// The session folder's tree, the file cache and the size and viewer rules of the Files screen.
// Desktop: `desktop/src/files/{tree,plan,cache,source}.rs`. Plain data, no widgets.

/// The daemon's largest frame (`femme_proto::codec::MAX_FRAME`).
const maxFrame = 16 << 20;

/// The most bytes one `FsRead` returns: a frame less 4 KiB for the envelope. Bigger files are not
/// offered.
const maxRead = maxFrame - 4096;

/// Files over this many bytes ask before they are read (the desktop asks over 8 MiB; a phone on
/// cellular earlier).
const askAbove = 2 << 20;

/// The text viewer takes files up to this size; bigger text goes to the hex view and share.
const textMax = 8 << 20;

/// How many bytes the hex view and the text sniff look at.
const headBytes = 4096;

/// A name the box may list and a path may carry: not empty, not `.` or `..`, no `/`.
bool validName(String name) =>
    name.isNotEmpty && name != '.' && name != '..' && !name.contains('/');

/// `parent/name`, relative to the session folder (`""` is the folder itself). `null` for a name
/// that could climb or jump: `..`, `.`, empty, with a `/` (so also a leading `/`).
String? joinPath(String parent, String name) {
  if (!validName(name)) return null;
  return parent.isEmpty ? name : '$parent/$name';
}

/// The folder `path` is in (`""` for a top-level name and for `""`).
String parentPath(String path) {
  final i = path.lastIndexOf('/');
  return i < 0 ? '' : path.substring(0, i);
}

/// The last segment of `path` (`""` for the root).
String baseName(String path) => path.substring(path.lastIndexOf('/') + 1);

/// The absolute path on the box: the session's cwd joined with `rel`.
String fullPath(String cwd, String rel) {
  if (rel.isEmpty) return cwd;
  return cwd.endsWith('/') ? '$cwd$rel' : '$cwd/$rel';
}

/// Dot-files and dot-folders, shown muted.
bool isHidden(String name) => name.startsWith('.');

/// `512 B`, `4.0 KiB`, `12.3 MiB`.
String humanSize(int len) {
  if (len < 1024) return '$len B';
  if (len < 1 << 20) return '${(len / 1024).toStringAsFixed(1)} KiB';
  if (len < 1 << 30) return '${(len / (1 << 20)).toStringAsFixed(1)} MiB';
  return '${(len / (1 << 30)).toStringAsFixed(1)} GiB';
}

/// The first step of opening a file (desktop `files/plan.rs`).
enum OpenPlan {
  /// The bytes are cached: show them.
  show,

  /// A read of it is out already: wait for it.
  wait,

  /// Bigger than one read: say so, do not offer.
  refuse,

  /// Big: wait for a yes.
  ask,

  /// Send `FsRead`.
  read,
}

/// What to do for a file of `len` bytes (as listed).
OpenPlan planOpen(
  int len, {
  bool cached = false,
  bool reading = false,
  bool confirmed = false,
}) {
  if (cached) return OpenPlan.show;
  if (reading) return OpenPlan.wait;
  if (len > maxRead) return OpenPlan.refuse;
  if (len > askAbove && !confirmed) return OpenPlan.ask;
  return OpenPlan.read;
}

enum ViewerKind { image, text, pdf, video, audio, binary }

const _image = {'png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp'};
const _video = {'mp4', 'mov', 'webm', 'm4v'};
const _audio = {'mp3', 'm4a', 'wav', 'aac', 'ogg', 'flac'};
const _text = {
  // prose and data
  'txt',
  'md',
  'markdown',
  'mdx',
  'rst',
  'adoc',
  'org',
  'tex',
  'log',
  'csv',
  'tsv',
  'json',
  'jsonl',
  'ndjson',
  'json5',
  'yaml',
  'yml',
  'toml',
  'ini',
  'cfg',
  'conf',
  'env',
  'properties',
  'xml',
  'svg',
  'plist',
  'lock',
  'sum',
  'mod',
  'diff',
  'patch',
  'srt', 'vtt', 'gitignore', 'gitattributes', 'editorconfig', 'dockerignore',
  // web
  'html', 'htm', 'css', 'scss', 'sass', 'less', 'js', 'mjs', 'cjs', 'jsx', 'ts',
  'tsx', 'mts', 'cts', 'vue', 'svelte', 'astro', 'graphql', 'gql',
  // systems
  'rs',
  'c',
  'h',
  'cc',
  'cpp',
  'cxx',
  'hpp',
  'hh',
  'hxx',
  'm',
  'mm',
  'go',
  'zig',
  'nim', 'd', 'asm', 's', 'ld', 'cmake', 'mk', 'make', 'ninja', 'bazel', 'bzl',
  'gradle', 'nix',
  // jvm, .net, mobile
  'java',
  'kt',
  'kts',
  'scala',
  'groovy',
  'clj',
  'cljs',
  'edn',
  'cs',
  'fs',
  'fsx',
  'vb', 'swift', 'dart', 'arb',
  // scripting
  'py',
  'pyi',
  'rb',
  'php',
  'pl',
  'pm',
  'lua',
  'r',
  'jl',
  'sh',
  'bash',
  'zsh',
  'fish',
  'ps1', 'bat', 'cmd', 'awk', 'sed', 'vim', 'el', 'lisp', 'scm', 'rkt',
  // functional and others
  'hs',
  'lhs',
  'ml',
  'mli',
  'ex',
  'exs',
  'erl',
  'hrl',
  'elm',
  'purs',
  'sql',
  'prisma',
  'proto',
  'thrift',
  'tf',
  'tfvars',
  'hcl',
  'sol',
  'v',
  'sv',
  'vhd',
  'glsl',
  'wgsl',
  'hlsl',
  'frag',
  'vert',
  'service',
  'desktop',
  'rules',
  'gd',
  'bashrc',
  'zshrc',
  'profile',
  'npmrc',
  'prettierrc',
  'eslintrc',
  'tscn',
  'tres',
};
const _textNames = {
  'makefile',
  'gnumakefile',
  'dockerfile',
  'containerfile',
  'license',
  'licence',
  'copying',
  'readme',
  'authors',
  'contributors',
  'changelog',
  'notice',
  'todo',
  'procfile',
  'gemfile',
  'rakefile',
  'brewfile',
  'justfile',
  'vagrantfile',
  'jenkinsfile',
  'caddyfile',
  'cmakelists.txt',
  'pkgbuild',
  'codeowners',
};

/// The viewer for a file, by its name.
ViewerKind viewerKind(String name) {
  final lower = name.toLowerCase();
  if (_textNames.contains(lower)) return ViewerKind.text;
  final dot = lower.lastIndexOf('.');
  if (dot < 0) return ViewerKind.binary;
  // `.gitignore`, `.env`: the whole dot-name is the extension. Other dot-files (`.bashrc`,
  // `.DS_Store`) are binary by name; the sniff after reading makes the text ones text.
  final ext = lower.substring(dot + 1);
  if (_image.contains(ext)) return ViewerKind.image;
  if (_video.contains(ext)) return ViewerKind.video;
  if (_audio.contains(ext)) return ViewerKind.audio;
  if (ext == 'pdf') return ViewerKind.pdf;
  if (_text.contains(ext)) return ViewerKind.text;
  return ViewerKind.binary;
}

/// The first [headBytes] decode as UTF-8 and hold no NUL. A cut at the end of the window may
/// split a character, so up to 3 trailing bytes are let go.
bool sniffText(List<int> bytes) {
  final n = bytes.length < headBytes ? bytes.length : headBytes;
  final head = bytes.sublist(0, n);
  if (head.contains(0)) return false;
  for (var cut = 0; cut <= 3 && cut <= n; cut++) {
    try {
      utf8.decode(head.sublist(0, n - cut));
      return true;
    } on FormatException {
      if (n < headBytes) return false; // the whole file was read: no excuse
    }
  }
  return false;
}

/// What the viewer shows once the bytes are in: the name's kind, a binary that sniffs as text
/// becomes text, text over [textMax] becomes hex.
ViewerKind shownKind(String name, List<int> bytes) {
  var kind = viewerKind(name);
  if (kind == ViewerKind.binary && bytes.isNotEmpty && sniffText(bytes)) {
    kind = ViewerKind.text;
  }
  if (kind == ViewerKind.text && bytes.length > textMax) {
    kind = ViewerKind.binary;
  }
  return kind;
}

/// The files read last, newest last, at most [budget] bytes; the oldest go first. A file bigger
/// than the budget alone is not kept.
class FileCache {
  FileCache({this.budget = 48 << 20});

  final int budget;
  final _items = <String, Uint8List>{}; // insertion-ordered: oldest first
  int _used = 0;

  int get used => _used;
  int get length => _items.length;
  Iterable<String> get paths => _items.keys;

  bool contains(String path) => _items.containsKey(path);

  /// The bytes of `path`, which becomes the newest.
  Uint8List? get(String path) {
    final b = _items.remove(path);
    if (b != null) _items[path] = b;
    return b;
  }

  void put(String path, Uint8List bytes) {
    final old = _items.remove(path);
    if (old != null) _used -= old.length;
    if (bytes.length > budget) return;
    while (_used + bytes.length > budget && _items.isNotEmpty) {
      final oldest = _items.keys.first;
      _used -= _items.remove(oldest)!.length;
    }
    _items[path] = bytes;
    _used += bytes.length;
  }

  void clear() {
    _items.clear();
    _used = 0;
  }
}

/// One listed folder: its entries once the box answered, the last error, a list in flight.
class DirState {
  List<FsEntry>? entries;
  Object? error;
  bool loading = false;
}

/// The session folder as the box lists it, folder by folder, when one is opened.
class FilesTree extends ChangeNotifier {
  FilesTree({required this.requester, required this.sid, FileCache? cache})
    : cache = cache ?? FileCache();

  final Requester requester;
  final String sid;
  final FileCache cache;

  final _dirs = <String, DirState>{};
  final _reads = <String, Future<Uint8List>>{};
  bool _disposed = false;

  /// The state of folder `path` (`""` = the session folder), `null` if never opened.
  DirState? dir(String path) => _dirs[path];

  /// The folders listed so far.
  Iterable<String> get loaded => _dirs.keys;

  /// Lists `path` unless it has been listed or is being listed.
  Future<void> ensure(String path) {
    final d = _dirs[path];
    if (d != null && (d.entries != null || d.loading)) return Future.value();
    return load(path);
  }

  /// Lists `path` (again).
  Future<void> load(String path) async {
    final d = _dirs.putIfAbsent(path, DirState.new);
    if (d.loading) return;
    d.loading = true;
    _notify();
    try {
      final resp = await requester.req(Req.fsList(sid, path), resend: false);
      d.entries = sortEntries(resp.fsEntries);
      d.error = null;
    } catch (e) {
      d.error = e;
    } finally {
      d.loading = false;
      _notify();
    }
  }

  /// Lists every opened folder again, and forgets the cached bytes.
  Future<void> refresh() {
    cache.clear();
    return Future.wait([for (final p in _dirs.keys.toList()) load(p)]);
  }

  /// A read of `path` is out.
  bool reading(String path) => _reads.containsKey(path);

  /// The bytes of `path`: from the cache, the read already out, or a new `FsRead`.
  Future<Uint8List> read(String path) {
    final hit = cache.get(path);
    if (hit != null) return Future.value(hit);
    return _reads[path] ??= () async {
      try {
        final resp = await requester.req(Req.fsRead(sid, path), resend: false);
        final data = resp.fsData;
        final bytes = data is Uint8List ? data : Uint8List.fromList(data);
        cache.put(path, bytes);
        return bytes;
      } finally {
        _reads.remove(path);
      }
    }();
  }

  void _notify() {
    if (!_disposed) notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    super.dispose();
  }
}

/// Entries with names a path may carry, folders first, then by name without regard to case.
List<FsEntry> sortEntries(List<FsEntry> entries) {
  final out = entries.where((e) => validName(e.name)).toList();
  out.sort((a, b) {
    if (a.dir != b.dir) return a.dir ? -1 : 1;
    final c = a.name.toLowerCase().compareTo(b.name.toLowerCase());
    return c != 0 ? c : a.name.compareTo(b.name);
  });
  return out;
}
