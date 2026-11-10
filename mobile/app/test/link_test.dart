import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/net/term_models.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

/// An in-process femmed stand-in speaking the `/femme/ws` envelope.
class FakeServer {
  late HttpServer _http;
  final sockets = <WebSocket>[];
  final hellos = <Map<String, dynamic>>[];

  /// Every request seen: (connection number, op, req).
  final reqs = <(int, String, Object)>[];
  final termUps = <Object>[];
  final _reqSeen = StreamController<(int, String, Object)>.broadcast();

  /// Answers a request, or returns null to leave it unanswered.
  Object? Function(int conn, Object req) answer = (_, _) => null;

  int get port => _http.port;

  Future<void> start() async {
    _http = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    _http.listen((r) async {
      if (r.uri.path != '/femme/ws' ||
          r.headers.value('authorization') != 'Bearer tok') {
        r.response.statusCode = HttpStatus.unauthorized;
        await r.response.close();
        return;
      }
      final ws = await WebSocketTransformer.upgrade(r);
      final conn = sockets.length;
      sockets.add(ws);
      ws.listen(
        (data) => _onFrame(
          conn,
          ws,
          jsonDecode(data as String) as Map<String, dynamic>,
        ),
      );
    });
  }

  void send(WebSocket ws, Object frame) => ws.add(jsonEncode(frame));

  void _onFrame(int conn, WebSocket ws, Map<String, dynamic> f) {
    final s = f['s'] as int;
    if (!f.containsKey('m')) return;
    final m = f['m'] as Map<String, dynamic>;
    if (s != 1) {
      termUps.add(m);
      if (m.containsKey('Open')) {
        send(ws, {
          's': s,
          'm': {
            'Frame': {
              'epoch': 1,
              'rev': 1,
              'base_rev': 0,
              'applied_seq': 0,
              'echo_seq': 0,
              'screen': 'Primary',
              'cols': 80,
              'rows': 24,
              'top_abs': 0,
              'history_lo_abs': 0,
              'cursor': {'row': 0, 'col': 0, 'shape': 'Block', 'visible': true},
              'modes': '',
              'title': null,
              'new_styles': [],
              'row_patches': [],
              'timing': null,
            },
          },
        });
      }
      return;
    }
    if (m.containsKey('Hello')) {
      hellos.add(m['Hello'] as Map<String, dynamic>);
      send(ws, {
        's': 1,
        'm': {
          'Welcome': {'proto': 1, 'server_version': '0.9.1', 'epoch': 7},
        },
      });
      send(ws, {'s': 2, 'open': 'Events'});
      send(ws, {
        's': 2,
        'm': changes('Global', 3, [
          {'Session': sessionJson(rev: 3)},
        ]),
      });
      return;
    }
    final r = m['Req'] as Map<String, dynamic>;
    final op = r['op'] as String;
    reqs.add((conn, op, r['req'] as Object));
    _reqSeen.add(reqs.last);
    final result = answer(conn, r['req'] as Object);
    if (result != null) {
      send(ws, {
        's': 1,
        'm': {
          'Resp': {'op': op, 'result': result},
        },
      });
    }
  }

  Future<(int, String, Object)> nextReq() => _reqSeen.stream.first;

  Future<void> stop() => _http.close(force: true);
}

Future<void> until(bool Function() cond) async {
  for (var i = 0; i < 200 && !cond(); i++) {
    await Future<void>.delayed(const Duration(milliseconds: 10));
  }
  expect(cond(), isTrue, reason: 'condition not met in time');
}

void main() {
  late FakeServer server;
  late Link link;

  setUp(() async {
    server = FakeServer();
    await server.start();
    link = Link(
      clientInstance: 4242,
      minBackoff: const Duration(milliseconds: 20),
      maxBackoff: const Duration(milliseconds: 100),
    );
  });

  tearDown(() async {
    await link.close();
    await server.stop();
  });

  test(
    'Hello → Welcome, Events open, a request answered, events delivered',
    () async {
      final events = <S2C>[];
      link.events.listen(events.add);
      int? welcomed;
      link.onWelcome = (e) => welcomed = e;
      link.cursors = () =>
          const Cursors(epoch: 7, globalRev: 2, sessions: {sid: 9});
      link.connect('ws://127.0.0.1:${server.port}', 'tok');
      await until(() => link.isOnline);

      expect(welcomed, 7);
      expect(link.state.value.serverVersion, '0.9.1');
      expect(server.hellos.single, {
        'proto': 1,
        'client_instance': 4242,
        'cursors': {
          'epoch': 7,
          'global_rev': 2,
          'sessions': [
            [sid, 9],
          ],
        },
      });
      await until(() => events.isNotEmpty);
      expect((events.single as Changes).rows.single, isA<SessionRow>());

      server.answer = (_, req) => req == 'ServerInfo'
          ? {
              'Ok': {
                'ServerInfo': {'version': '0.9.1', 'deployed': 'df40bc9'},
              },
            }
          : {
              'Err': {'code': 'Bad', 'msg': 'no'},
            };
      final info = await link.req(Req.serverInfo);
      expect(info.serverInfo.deployed, 'df40bc9');
      await expectLater(link.req(Req.prs), throwsA(isA<FemmeError>()));
    },
  );

  test('an unanswered request is resent with the same op after the server drops the socket', () async {
    link.connect('ws://127.0.0.1:${server.port}', 'tok');
    await until(() => link.isOnline);

    final seen = server.nextReq();
    final prompt = link.req(Req.prompt(sid, 'hi'));
    final sub = link.req(
      Req.subscribe(const Scope.session(sid)),
      resend: false,
    );
    final (conn0, op0, _) = await seen;
    await until(() => server.reqs.length == 2);
    expect(conn0, 0);

    // Second connection: answer everything.
    server.answer = (_, _) => {'Ok': 'Ok'};
    await server.sockets.first.close();

    await expectLater(sub, throwsA(isA<LinkLost>()));
    expect((await prompt).isOk, isTrue);
    expect(server.hellos.length, 2);
    final resent = server.reqs.where((r) => r.$1 == 1).toList();
    expect(resent.length, 1, reason: 'only the resendable request goes again');
    expect(resent.single.$2, op0);
    expect(resent.single.$3, Req.prompt(sid, 'hi'));
  });

  test('a request made while offline waits for the link', () async {
    server.answer = (_, _) => {'Ok': 'Ok'};
    final f = link.req(Req.interrupt(sid));
    link.connect(
      '127.0.0.1:${server.port}',
      'tok',
    ); // bare host: wss, which fails here
    await until(
      () =>
          link.state.value.phase == LinkPhase.offline &&
          link.state.value.retryAt != null,
    );
    link.connect('ws://127.0.0.1:${server.port}/femme/ws', 'tok');
    expect((await f).isOk, isTrue);
  });

  test('a wrong token keeps the link offline with a reason', () async {
    link.connect('ws://127.0.0.1:${server.port}', 'nope');
    await until(() => link.state.value.retryAt != null);
    expect(link.state.value.phase, LinkPhase.offline);
    expect(link.state.value.reason, isNotEmpty);
  });

  test('a term stream opens, sends Open and receives a frame', () async {
    link.connect('ws://127.0.0.1:${server.port}', 'tok');
    await until(() => link.isOnline);
    final t = link.openTerm(const PaneId(sid, 0));
    final first = await t.down.first;
    expect((first as FrameDown).frame.cols, 80);
    expect(server.termUps.single, {
      'Open': {
        'pane': {'sid': sid, 'no': 0},
        'epoch': 0,
        'have_rev': 0,
        'client_instance': 4242,
      },
    });
    t.send(TermUp.resize(100, 30));
    await until(() => server.termUps.length == 2);
    t.close();
    expect(await t.closed, isNull);
  });

  test('linkUrl', () {
    expect(
      linkUrl('box.example.com').toString(),
      'wss://box.example.com/femme/ws',
    );
    expect(
      linkUrl('https://box.example.com').toString(),
      'wss://box.example.com/femme/ws',
    );
    expect(
      linkUrl('ws://127.0.0.1:8787').toString(),
      'ws://127.0.0.1:8787/femme/ws',
    );
    expect(linkUrl('ws://h/custom').toString(), 'ws://h/custom');
  });
}
