import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/box/pr_detail.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

Map<String, Object?> prJson(String title) => {
  'number': 9,
  'title': title,
  'branch': 'b',
  'author': 'a',
  'url': 'https://github.com/o/r/pull/9',
  'body': '',
  'additions': 0,
  'deletions': 0,
  'video': null,
  'mergeable': true,
  'fixing': null,
  'merging': false,
};

class _BoxLink extends Link {
  _BoxLink() : super(clientInstance: 1);
  List<Map<String, Object?>> prs = [prJson('first')];
  int reads = 0;

  @override
  bool get isOnline => true;

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
    bool track = true,
  }) async {
    if (req == Req.prs) {
      reads++;
      return Resp('Prs', prs);
    }
    throw StateError('unexpected $req');
  }
}

void main() {
  testWidgets('the detail links to GitHub and re-reads its PR every 10 s', (
    t,
  ) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    final link = _BoxLink();
    await t.pumpWidget(
      Femme(
        store: Store(),
        link: link,
        prefs: prefs,
        child: CupertinoApp(
          home: PrDetailScreen(pr: Pr.fromJson(prJson('first'))),
        ),
      ),
    );
    await t.pump();
    expect(find.text('Open on GitHub'), findsOneWidget);
    expect(find.text('https://github.com/o/r/pull/9'), findsOneWidget);
    expect(link.reads, 1);

    link.prs = [prJson('renamed')];
    await t.pump(const Duration(seconds: 10));
    await t.pump();
    expect(link.reads, 2);
    expect(find.text('renamed'), findsOneWidget);

    link.prs = [];
    await t.pump(const Duration(seconds: 10));
    await t.pump();
    expect(find.text('no longer open'), findsOneWidget);

    await t.pumpWidget(const SizedBox());
  });
}
