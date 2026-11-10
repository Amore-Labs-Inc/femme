import 'dart:convert';
import 'dart:math';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/push.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';

class FakeKeeper implements PushKeeper {
  @override
  String? pushTopic;
  @override
  bool pushOn = false;
  @override
  String? pushRulesJson;
  @override
  Future<void> setPush({String? topic, bool? on, String? rulesJson}) async {
    if (topic != null) pushTopic = topic;
    if (on != null) pushOn = on;
    if (rulesJson != null) pushRulesJson = rulesJson;
  }
}

void main() {
  test('rules round-trip and encode as the daemon expects', () {
    const r = PushRules(
      blocked: true,
      done: false,
      quiet: (23, 7),
      muted: {'B', 'A'},
    );
    final j = jsonDecode(jsonEncode(r.toJson()));
    expect(j, {
      'blocked': true,
      'done': false,
      'muted': ['A', 'B'],
      'quiet': [23, 7],
    });
    expect(PushRules.fromJson(j), r);
    expect(PushRules.fromJson(null), const PushRules());
    expect(jsonDecode(jsonEncode(Req.pushRegister('femme-x', r))), {
      'PushRegister': {'platform': 'Ntfy', 'token': 'femme-x', 'rules': j},
    });
    expect(Req.pushUnregister('femme-x'), {
      'PushUnregister': {'token': 'femme-x'},
    });
  });

  test('a topic is femme- and 20 base32 characters', () {
    final t = newTopic(Random(1));
    expect(t, matches(RegExp(r'^femme-[a-z2-7]{20}$')));
    expect(newTopic(Random(2)), isNot(t));
  });

  group('PushSync', () {
    late ValueNotifier<LinkState> link;
    late FakeKeeper keeper;
    late List<Object> sent;
    Object? fail;

    Future<Resp> req(Object r) async {
      sent.add(r);
      if (fail != null) throw fail!;
      return const Resp('Ok', null);
    }

    setUp(() {
      link = ValueNotifier(const LinkState(LinkPhase.offline));
      keeper = FakeKeeper();
      sent = [];
      fail = null;
    });

    test('turning push on mints a topic and registers once online', () async {
      final p = PushSync(req, link, keeper);
      await p.setOn(true);
      expect(keeper.pushTopic, startsWith('femme-'));
      expect(sent, isEmpty, reason: 'offline: nothing sent yet');
      link.value = const LinkState(LinkPhase.online);
      await Future<void>.delayed(Duration.zero);
      expect(
        sent.single,
        Req.pushRegister(keeper.pushTopic!, const PushRules()),
      );
      expect(p.registered, keeper.pushTopic);
      link.value = const LinkState(LinkPhase.offline);
      link.value = const LinkState(LinkPhase.online);
      await Future<void>.delayed(Duration.zero);
      expect(sent, hasLength(1), reason: 'same settings: not sent again');
    });

    test(
      'a rule change re-registers; off unregisters the confirmed topic',
      () async {
        link.value = const LinkState(LinkPhase.online);
        keeper
          ..pushOn = true
          ..pushTopic = 'femme-t';
        final p = PushSync(req, link, keeper);
        await Future<void>.delayed(Duration.zero);
        expect(sent, hasLength(1));
        await p.setRules(const PushRules(done: false));
        expect(sent, hasLength(2));
        expect((sent.last as Map)['PushRegister'], isNotNull);
        await p.setOn(false);
        expect(sent.last, Req.pushUnregister('femme-t'));
        expect(p.registered, isNull);
      },
    );

    test('a failure is kept and retried on the next connection', () async {
      link.value = const LinkState(LinkPhase.online);
      keeper
        ..pushOn = true
        ..pushTopic = 'femme-t';
      fail = const FemmeError('Unsupported', 'no push');
      final p = PushSync(req, link, keeper);
      await Future<void>.delayed(Duration.zero);
      expect(p.unsupported, isTrue);
      expect(p.registered, isNull);
      fail = null;
      link.value = const LinkState(LinkPhase.offline);
      link.value = const LinkState(LinkPhase.online);
      await Future<void>.delayed(Duration.zero);
      expect(p.error, isNull);
      expect(p.registered, 'femme-t');
    });
  });
}
