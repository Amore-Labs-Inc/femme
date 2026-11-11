import 'package:femme_mobile/ui/chat/markdown.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

Widget _app(Widget child) => CupertinoApp(
  theme: femmeTheme(Brightness.light),
  home: CupertinoPageScaffold(child: SingleChildScrollView(child: child)),
);

bool _shows(String s) =>
    find.textContaining(s, findRichText: true).evaluate().isNotEmpty;

void main() {
  test('an open fence is closed at the end, a closed one is left alone', () {
    expect(
      closeOpen('see\n\n```rust\nfn main'),
      'see\n\n```rust\nfn main\n```',
    );
    expect(closeOpen('```\na\n'), '```\na\n```');
    expect(closeOpen('~~~~\na\n~~~'), '~~~~\na\n~~~\n~~~~');
    const done = 'x\n\n```py\nprint(1)\n```\n\nafter';
    expect(closeOpen(done), done);
    // a backtick in a backtick info string is not a fence
    expect(closeOpen('``` a`b'), '``` a`b');
  });

  test(
    'a lone ** in the last paragraph is literal, pairs and code are kept',
    () {
      expect(closeOpen('half **bold'), r'half \*\*bold');
      expect(closeOpen('**a** and **b'), r'**a** and \*\*b');
      expect(closeOpen('**a** done'), '**a** done');
      expect(closeOpen('`**` in code'), '`**` in code');
      // an earlier paragraph is not touched
      expect(closeOpen('one **x\n\ntwo'), 'one **x\n\ntwo');
      // inside an open fence nothing is escaped, the fence is closed
      expect(closeOpen('```\n**'), '```\n**\n```');
    },
  );

  testWidgets('a fenced block shows its language; one without shows none', (
    tester,
  ) async {
    await tester.pumpWidget(
      _app(const Md('```rust\nfn main() {}\n```\n\n```\nplain\n```')),
    );
    expect(find.text('rust'), findsOneWidget);
    expect(find.text('fn main() {}'), findsOneWidget);
    expect(find.text('plain'), findsOneWidget);
  });

  testWidgets('streaming text with an open fence keeps what follows it', (
    tester,
  ) async {
    await tester.pumpWidget(
      _app(const Md('before\n\n```sh\nls -la\nmore', streaming: true)),
    );
    expect(_shows('before'), isTrue);
    expect(find.text('sh'), findsOneWidget);
    expect(find.text('ls -la\nmore'), findsOneWidget);
    await tester.pumpWidget(_app(const Md('the **fix is', streaming: true)));
    expect(_shows('the **fix is'), isTrue);
  });
}
