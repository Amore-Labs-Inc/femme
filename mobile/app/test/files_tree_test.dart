import 'dart:async';
import 'dart:typed_data';

import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/files.dart';
import 'package:flutter_test/flutter_test.dart';

import 'files_fake.dart';

FakeBox box() => FakeBox(
  dirs: {
    '': const [
      FsEntry('zeta.txt', false, 3),
      FsEntry('src', true, 0),
      FsEntry('Alpha.md', false, 10),
      FsEntry('.git', true, 0),
      FsEntry('..', true, 0),
      FsEntry('a/b', false, 1),
    ],
    'src': const [FsEntry('main.rs', false, 5)],
  },
  files: {
    'zeta.txt': [1, 2, 3],
    'src/main.rs': [102, 110, 32, 109, 0],
  },
);

void main() {
  test('lists lazily, sorted dirs first, bad names dropped', () async {
    final b = box();
    final t = FilesTree(requester: b, sid: 's1');
    expect(b.sent, isEmpty);
    await t.ensure('');
    expect(b.sent.single, {
      'FsList': {'sid': 's1', 'path': ''},
    });
    expect(t.dir('')!.entries!.map((e) => e.name), [
      '.git',
      'src',
      'Alpha.md',
      'zeta.txt',
    ]);
    expect(t.dir('src'), isNull, reason: 'not opened yet');
    await t.ensure('');
    expect(b.listed, [''], reason: 'listed once');
    await t.ensure('src');
    expect(b.listed, ['', 'src']);
  });

  test('refresh lists every opened folder again', () async {
    final b = box();
    final t = FilesTree(requester: b, sid: 's1');
    await t.ensure('');
    await t.ensure('src');
    b.dirs = {
      ...b.dirs,
      'src': const [FsEntry('lib.rs', false, 1), FsEntry('main.rs', false, 5)],
    };
    await t.refresh();
    expect(b.listed, ['', 'src', '', 'src']);
    expect(t.dir('src')!.entries!.first.name, 'lib.rs');
  });

  test('errors stay with their folder; a retry clears them', () async {
    final b = box()..failing.add('src');
    final t = FilesTree(requester: b, sid: 's1');
    await t.ensure('');
    await t.ensure('src');
    expect(t.dir('src')!.error, isA<FemmeError>());
    expect(t.dir('')!.error, isNull);
    b.failing.clear();
    await t.load('src');
    expect(t.dir('src')!.error, isNull);
    expect(t.dir('src')!.entries, hasLength(1));
  });

  test('one list in flight per folder, notifies', () async {
    final b = box()..gate = Completer();
    final t = FilesTree(requester: b, sid: 's1');
    var notes = 0;
    t.addListener(() => notes++);
    final f1 = t.ensure('');
    final f2 = t.ensure('');
    expect(t.dir('')!.loading, isTrue);
    b.gate!.complete();
    await Future.wait([f1, f2]);
    expect(b.listed, ['']);
    expect(notes, 2);
  });

  test('reads are cached and shared while in flight', () async {
    final b = box()..gate = Completer();
    final t = FilesTree(requester: b, sid: 's1');
    final r1 = t.read('src/main.rs');
    final r2 = t.read('src/main.rs');
    expect(t.reading('src/main.rs'), isTrue);
    b.gate!.complete();
    expect(await r1, [102, 110, 32, 109, 0]);
    expect(await r2, [102, 110, 32, 109, 0]);
    expect(b.read, ['src/main.rs']);
    expect(t.reading('src/main.rs'), isFalse);
    await t.read('src/main.rs');
    expect(b.read, ['src/main.rs'], reason: 'from the cache');
    await expectLater(t.read('missing'), throwsA(isA<FemmeError>()));
    expect(t.reading('missing'), isFalse);
  });

  group('FileCache', () {
    Uint8List mib(int n) => Uint8List(n << 20);

    test('evicts the least recently used past 48 MiB', () {
      final c = FileCache();
      expect(c.budget, 48 << 20);
      c.put('a', mib(20));
      c.put('b', mib(20));
      expect(c.get('a'), isNotNull); // a is now newer than b
      c.put('c', mib(20));
      expect(c.paths, ['a', 'c']);
      expect(c.used, 40 << 20);
    });

    test('a file over the budget is not kept; replacing counts once', () {
      final c = FileCache();
      c.put('a', mib(10));
      c.put('huge', Uint8List((48 << 20) + 1));
      expect(c.contains('huge'), isFalse);
      expect(c.paths, ['a']);
      c.put('a', mib(5));
      expect(c.used, 5 << 20);
      c.put('full', mib(48));
      expect(c.paths, ['full']);
      expect(c.used, 48 << 20);
    });
  });
}
