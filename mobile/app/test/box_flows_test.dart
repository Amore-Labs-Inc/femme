import 'dart:convert';

import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/box_flows.dart';
import 'package:femme_mobile/ui/box/add_account.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

/// A fake board: answers each `BoardApi` call with the next scripted (status, body) for its
/// `METHOD path`, and records what was sent.
class FakeBoard {
  final answers = <String, List<(int, String)>>{};

  /// (method, path, body as sent: JSON text, or null for none).
  final sent = <(String, String, String?)>[];

  void on(String call, int status, Object body) =>
      (answers[call] ??= []).add((status, jsonEncode(body)));

  Future<Resp> req(Object r) async {
    final b = (r as Map)['BoardApi'] as Map;
    final call = '${b['method']} ${b['path']}';
    final body = b['body'] as String;
    sent.add((
      b['method'] as String,
      b['path'] as String,
      body.isEmpty ? null : body,
    ));
    final q = answers[call];
    if (q == null || q.isEmpty) throw StateError('no answer for $call');
    final a = q.length > 1 ? q.removeAt(0) : q.first;
    return Resp('BoardApi', {'status': a.$1, 'body': a.$2});
  }
}

const link = 'https://claude.example.test/cai/oauth/authorize?code=true';
Map<String, Object?> accounts({String? login}) => {
  'cswap': {'accounts': []},
  'aws': {},
  'login': login == null ? {} : {'claude': login},
};

Map<String, Object?> key({
  String account = 'the box',
  String? code,
  String? error,
}) => {
  'name': 'ci',
  'hint': 'a1b2',
  'created': 1793000000,
  'account': account,
  'github': code == null
      ? null
      : {'code': code, 'url': 'https://github.com/login/device'},
  'error': error,
};

void main() {
  test('login link: login.claude, or none', () {
    expect(parseLoginLink(jsonEncode(accounts(login: link))), link);
    expect(parseLoginLink(jsonEncode(accounts())), isNull);
    expect(parseLoginLink('{"login":null}'), isNull);
    expect(parseLoginLink('nope'), isNull);
  });

  group('LoginFlow', () {
    test('start, then the code adds the account', () async {
      final b = FakeBoard()
        ..on('POST /api/accounts/claude/login', 200, accounts(login: link))
        ..on('POST /api/accounts/claude/code', 200, accounts());
      final f = LoginFlow(b.req);
      await f.start();
      expect(f.phase, LoginPhase.link);
      expect(f.link, link);
      expect(await f.submit('  abc#def \n'), isTrue);
      expect(f.phase, LoginPhase.done);
      expect(b.sent, [
        ('POST', '/api/accounts/claude/login', '{}'),
        ('POST', '/api/accounts/claude/code', '{"code":"abc#def"}'),
      ]);
    });

    test('an empty code is not sent', () async {
      final b = FakeBoard();
      final f = LoginFlow(b.req, link: link);
      expect(await f.submit('  '), isFalse);
      expect(f.error, 'paste the code the sign-in page shows');
      expect(f.phase, LoginPhase.link);
      expect(b.sent, isEmpty);
    });

    test('a refused code: still on while the box keeps the link', () async {
      final b = FakeBoard()
        ..on('POST /api/accounts/claude/code', 400, {
          'error': 'paste the code exactly as the sign-in page shows it',
        })
        ..on('GET /api/accounts', 200, accounts(login: link));
      final f = LoginFlow(b.req, link: link);
      expect(await f.submit('x'), isFalse);
      expect(f.phase, LoginPhase.link);
      expect(f.error, 'paste the code exactly as the sign-in page shows it');
    });

    test('a refused code that ended the sign-in', () async {
      final b = FakeBoard()
        ..on('POST /api/accounts/claude/code', 502, {'error': 'bad code'})
        ..on('GET /api/accounts', 200, accounts());
      final f = LoginFlow(b.req, link: link);
      await f.submit('x');
      expect(f.phase, LoginPhase.ended);
      expect(f.error, 'bad code');
    });

    test('no link yet: refresh finds it', () async {
      final b = FakeBoard()
        ..on('POST /api/accounts/claude/login', 200, accounts())
        ..on('GET /api/accounts', 200, accounts(login: link));
      final f = LoginFlow(b.req);
      await f.start();
      expect(f.phase, LoginPhase.waitingLink);
      await f.refresh();
      expect(f.phase, LoginPhase.link);
    });

    test('cancel deletes the sign-in; a failed start says why', () async {
      final b = FakeBoard()
        ..on('DELETE /api/accounts/claude/login', 200, accounts());
      final f = LoginFlow(b.req, link: link);
      await f.cancel();
      expect(f.phase, LoginPhase.cancelled);
      expect(b.sent.single, ('DELETE', '/api/accounts/claude/login', null));

      final g = LoginFlow(
        (FakeBoard()..on('POST /api/accounts/claude/login', 502, {
              'error': 'claude: not found',
            }))
            .req,
      );
      await g.start();
      expect(g.phase, LoginPhase.ended);
      expect(g.error, 'claude: not found');
      // Never started: nothing to delete.
      final h = FakeBoard();
      await LoginFlow(h.req).cancel();
      expect(h.sent, isEmpty);
    });
  });

  testWidgets('add account sheet: link, code, Add account', (t) async {
    final b = FakeBoard()
      ..on('POST /api/accounts/claude/login', 200, accounts(login: link))
      ..on('POST /api/accounts/claude/code', 200, accounts());
    await t.pumpWidget(CupertinoApp(home: AddAccountSheet(req: b.req)));
    await t.pumpAndSettle();
    expect(find.text(link), findsOneWidget);
    expect(find.text('Open link'), findsOneWidget);
    await t.enterText(find.byType(CupertinoTextField), 'abc#def');
    await t.tap(find.text('Add account'));
    await t.pump();
    expect(b.sent.last, (
      'POST',
      '/api/accounts/claude/code',
      '{"code":"abc#def"}',
    ));
  });
}
