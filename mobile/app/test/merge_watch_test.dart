import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/box_watch.dart';
import 'package:flutter_test/flutter_test.dart';

Map<String, Object?> prJson(int n, {bool merging = false}) => {
  'number': n,
  'title': 't',
  'branch': 'b',
  'author': 'a',
  'url': 'https://github.com/o/r/pull/$n',
  'body': '',
  'additions': 0,
  'deletions': 0,
  'video': null,
  'mergeable': merging ? false : true,
  'fixing': null,
  'merging': merging,
};

/// A fake daemon: `ServerInfo` answers [sha] (or fails while null and [infoFails]), `Prs` the list.
class Box {
  String? sha = 'old';
  bool infoFails = false;
  bool prsFails = false;
  List<Map<String, Object?>> prs = [];
  final calls = <Object>[];

  Future<Resp> req(Object r) async {
    calls.add(r);
    if (r == Req.serverInfo) {
      if (infoFails) throw const LinkLost('restarting');
      return Resp('ServerInfo', {'version': '1', 'deployed': sha});
    }
    if (r == Req.prs) {
      if (prsFails) throw const LinkLost('restarting');
      return Resp('Prs', prs);
    }
    throw StateError('unexpected $r');
  }
}

void main() {
  testWidgets('a clean merge: deploying… until the box runs a new commit', (
    t,
  ) async {
    final box = Box();
    final w = MergeWatch(box.req);
    await w.merged(3, 'old');
    expect(w.of(3), isA<MergeDeploying>());
    expect(w.busy(3), isTrue);
    await t.pump(const Duration(seconds: 5));
    await t.pump();
    expect(mergeLine(w.of(3)!, w.poll).$1, '#3 merged · deploying… 5s');
    box.sha = 'abcdef0123';
    await t.pump(const Duration(seconds: 5));
    await t.pump();
    expect(w.of(3), isA<MergeDeployed>());
    expect(mergeLine(w.of(3)!, w.poll), ('#3 deployed abcdef0', true));
    expect(w.busy(3), isFalse);
    w.dispose();
  });

  testWidgets('an unread commit before the merge is not a deploy', (t) async {
    final box = Box();
    final w = MergeWatch(box.req);
    await w.merged(3, null);
    await t.pump(const Duration(seconds: 5));
    await t.pump();
    expect(
      w.of(3),
      isA<MergeDeploying>().having((s) => s.from, 'from', 'old'),
      reason: 'the first answer is the baseline',
    );
    box.sha = 'new';
    await t.pump(const Duration(seconds: 5));
    await t.pump();
    expect(w.of(3), isA<MergeDeployed>());
    w.dispose();
  });

  testWidgets('gives up after ten minutes on the old commit', (t) async {
    final box = Box()..infoFails = true;
    final w = MergeWatch(box.req);
    await w.merged(3, 'old');
    for (var i = 0; i < 120; i++) {
      await t.pump(const Duration(seconds: 5));
    }
    await t.pump();
    expect(mergeLine(w.of(3)!, w.poll), (
      '#3 merged, but the box still runs the old commit after 10 min',
      false,
    ));
    w.dispose();
  });

  testWidgets(
    'a queued merge is watched until it leaves the list, then deployed',
    (t) async {
      final box = Box()..prs = [prJson(4, merging: true), prJson(5)];
      final w = MergeWatch(box.req);
      await w.merged(4, 'old');
      expect(w.of(4), isA<MergeQueued>());
      expect(
        mergeLine(w.of(4)!, w.poll).$1,
        '#4 conflicts with main: Claude resolves them, then it merges and deploys by itself',
      );
      expect(w.prs.value!.length, 2);

      await t.pump(const Duration(seconds: 10));
      await t.pump();
      expect(w.of(4), isA<MergeQueued>(), reason: 'still merging');

      box
        ..prs = [prJson(5)]
        ..sha = 'merged-main';
      await t.pump(const Duration(seconds: 10));
      await t.pump();
      expect(
        w.of(4),
        isA<MergeDeploying>().having((s) => s.from, 'from', 'merged-main'),
      );
      final reads = box.calls.where((c) => c == Req.prs).length;
      await t.pump(const Duration(seconds: 30));
      expect(
        box.calls.where((c) => c == Req.prs).length,
        reads,
        reason: 'the list watch stops once nothing is queued',
      );
      box.sha = 'deployed';
      await t.pump(const Duration(seconds: 5));
      await t.pump();
      expect(w.of(4), isA<MergeDeployed>());
      w.dispose();
    },
  );

  testWidgets('a queued merge whose fix stopped says so', (t) async {
    final box = Box()..prs = [prJson(4, merging: true)];
    final w = MergeWatch(box.req);
    await w.merged(4, 'old');
    box.prs = [prJson(4)];
    await t.pump(const Duration(seconds: 10));
    await t.pump();
    expect(mergeLine(w.of(4)!, w.poll), (
      '#4 not merged: the conflict fix stopped',
      false,
    ));
    w.dispose();
  });

  testWidgets('no list after the merge: it most likely merged', (t) async {
    final box = Box()..prsFails = true;
    final w = MergeWatch(box.req);
    await w.merged(6, 'old');
    expect(w.of(6), isA<MergeDeploying>());
    w.dispose();
    await t.pump(const Duration(seconds: 5));
  });
}
