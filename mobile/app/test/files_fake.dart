import 'dart:async';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';

/// A box with a folder tree in memory: `FsList` answers from [dirs], `FsRead` from [files].
class FakeBox implements Requester {
  FakeBox({this.dirs = const {}, this.files = const {}});

  /// Folder path → entries (`name`, `dir`, `len`).
  Map<String, List<FsEntry>> dirs;
  Map<String, List<int>> files;

  /// Every request, in order (the `FsList` / `FsRead` maps).
  final sent = <Object>[];

  /// When set, requests wait on it.
  Completer<void>? gate;

  /// Paths whose `FsList` fails.
  final failing = <String>{};

  @override
  bool isOnline = true;

  List<String> get listed => [
    for (final r in sent)
      if (r is Map && r['FsList'] != null)
        (r['FsList'] as Map)['path'] as String,
  ];
  List<String> get read => [
    for (final r in sent)
      if (r is Map && r['FsRead'] != null)
        (r['FsRead'] as Map)['path'] as String,
  ];

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) async {
    sent.add(req);
    if (gate != null) await gate!.future;
    final m = req as Map;
    if (m['FsList'] != null) {
      final path = (m['FsList'] as Map)['path'] as String;
      if (failing.contains(path)) throw const FemmeError('NotFound', 'gone');
      final entries = dirs[path];
      if (entries == null) throw const FemmeError('NotFound', 'no such dir');
      return Resp('FsEntries', [
        for (final e in entries) {'name': e.name, 'dir': e.dir, 'len': e.len},
      ]);
    }
    final path = (m['FsRead'] as Map)['path'] as String;
    final data = files[path];
    if (data == null) throw const FemmeError('NotFound', 'no such file');
    return Resp('FsData', data);
  }
}

/// Not a test: `flutter test test/files_*` loads every match, so this file needs a `main`.
void main() {}
