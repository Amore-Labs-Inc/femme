import 'dart:convert';

import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/box_flows.dart';
import 'package:femme_mobile/ui/box/add_account.dart';
import 'package:femme_mobile/ui/box/github.dart';
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
  test('path segments keep a name inside its segment', () {
    expect(pathSegment('ci-bot'), 'ci-bot');
    expect(pathSegment('../accounts/x y'), '..%2Faccounts%2Fx%20y');
    expect(pathSegment('é'), '%C3%A9');
    expect(pathSegment('a?b#c'), 'a%3Fb%23c');
  });

  test('meta: the scheduler line says where the box moves on', () {
    expect(
      schedulerLine(parseMeta('{}')),
      'switches account at its cap · agents pause when every account is past it',
    );
    final m = parseMeta(
      '{"cap":60,"state":"working (31% used, pause at 60%)","deploy":null}',
    );
    expect(m.cap, 60);
    expect(
      schedulerLine(m),
      'switches account at 60% · agents pause when every account is past it'
      ' · working (31% used, pause at 60%)',
    );
    expect(parseMeta('not json').cap, isNull);
    expect(parseMeta('{"cap":"60","state":""}').state, isNull);
  });

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

  test('keys: each one\'s GitHub side', () {
    final g = parseKeysGithub(
      jsonEncode({
        'keys': [
          key(code: 'ABCD-1234'),
          {'name': 'x', 'error': 'gh: denied'},
          {'nameless': 1},
        ],
      }),
    );
    expect(g.keys, ['ci', 'x']);
    expect(g['ci']!.device!.code, 'ABCD-1234');
    expect(g['ci']!.device!.url, 'https://github.com/login/device');
    expect(g['x']!.device, isNull);
    expect(g['x']!.account, 'the box');
    expect(g['x']!.error, 'gh: denied');
    expect(parseKeysGithub('[]'), isEmpty);
    expect(parseKeysGithub('oops'), isEmpty);
  });

  group('GithubFlow', () {
    test('code, then connected once the box settles it', () async {
      final b = FakeBoard()
        ..on('POST /api/keys/ci/github', 200, [key(code: 'ABCD-1234')])
        ..on('GET /api/keys', 200, [key(code: 'ABCD-1234')])
        ..on('GET /api/keys', 200, [key(account: 'Ada (ada)')]);
      final f = GithubFlow(b.req, 'ci');
      await f.start(accountBefore: 'the box');
      expect(f.phase, GithubPhase.code);
      expect(f.device!.code, 'ABCD-1234');
      expect(f.polling, isTrue);
      await f.poll();
      expect(f.phase, GithubPhase.code);
      await f.poll();
      expect(f.phase, GithubPhase.connected);
      expect(f.account, 'Ada (ada)');
      expect(f.polling, isFalse);
      await f.poll(); // no more reads
      expect(b.sent.map((s) => s.$2), [
        '/api/keys/ci/github',
        '/api/keys',
        '/api/keys',
      ]);
      expect(b.sent.first.$3, '{}');
    });

    test('no code yet: waits, then shows it', () async {
      final b = FakeBoard()
        ..on('POST /api/keys/ci/github', 200, [key()])
        ..on('GET /api/keys', 200, [key(code: 'WXYZ-9876')]);
      final f = GithubFlow(b.req, 'ci');
      await f.start(accountBefore: 'the box');
      expect(f.phase, GithubPhase.waiting);
      await f.poll();
      expect(f.phase, GithubPhase.code);
    });

    test('a failed sign-in, a revoked key, a refused start', () async {
      final b = FakeBoard()
        ..on('POST /api/keys/ci/github', 200, [key(code: 'ABCD-1234')])
        ..on('GET /api/keys', 200, [key(error: 'gh: expired')]);
      final f = GithubFlow(b.req, 'ci');
      await f.start();
      await f.poll();
      expect(f.phase, GithubPhase.failed);
      expect(f.error, 'gh: expired');

      final c = FakeBoard()
        ..on('POST /api/keys/ci/github', 200, [key(code: 'ABCD-1234')])
        ..on('GET /api/keys', 200, []);
      final g = GithubFlow(c.req, 'ci');
      await g.start();
      await g.poll();
      expect(g.phase, GithubPhase.failed);
      expect(g.error, 'the key is gone');

      final d = FakeBoard()
        ..on('POST /api/keys/ci/github', 404, {'error': 'not found'});
      final h = GithubFlow(d.req, 'ci');
      await h.start();
      expect(h.phase, GithubPhase.failed);
      expect(h.error, 'not found');
    });

    test(
      'cancel deletes the sign-in, and a late read changes nothing',
      () async {
        final b = FakeBoard()
          ..on('DELETE /api/keys/a%20b/github', 200, [])
          ..on('GET /api/keys', 200, []);
        final f = GithubFlow(b.req, 'a b')
          ..resume(
            const GithubDevice('ABCD-1234', 'https://github.com/login/device'),
            'the box',
          );
        expect(f.phase, GithubPhase.code);
        await f.cancel();
        expect(f.phase, GithubPhase.cancelled);
        expect(b.sent.single, ('DELETE', '/api/keys/a%20b/github', null));
        await f.poll();
        expect(f.phase, GithubPhase.cancelled);
      },
    );
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

  testWidgets('GitHub sheet shows the code and cancels', (t) async {
    final b = FakeBoard()
      ..on('POST /api/keys/ci/github', 200, [key(code: 'ABCD-1234')])
      ..on('GET /api/keys', 200, [key(code: 'ABCD-1234')])
      ..on('DELETE /api/keys/ci/github', 200, [key()]);
    await t.pumpWidget(
      CupertinoApp(
        home: GithubSheet(
          req: b.req,
          keyGithub: const KeyGithub('ci', 'the box', null, null),
        ),
      ),
    );
    await t.pump();
    expect(find.text('ABCD-1234'), findsOneWidget);
    expect(find.text('Open github.com'), findsOneWidget);
    await t.pump(const Duration(seconds: 3));
    expect(b.sent.last.$2, '/api/keys');
    await t.tap(find.text('Cancel'));
    await t.pump();
    expect(b.sent.last, ('DELETE', '/api/keys/ci/github', null));
    await t.pumpWidget(const SizedBox());
  });
}
