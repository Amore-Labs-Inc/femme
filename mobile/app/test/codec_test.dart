import 'dart:convert';

import 'package:femme_mobile/net/ids.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/net/wire.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

/// Through a real JSON text and back, as on the wire.
Object? wire(Object? v) => jsonDecode(jsonEncode(v));

void main() {
  test('Session round-trips', () {
    final j = sessionJson();
    final s = Session.fromJson(wire(j) as Json);
    expect(s.status, SessionStatus.working);
    expect(s.goal!.state, GoalState.notMet);
    expect(s.context!.fraction, closeTo(0.406, 0.001));
    expect(s.title, 'lovable-lesson');
    expect(wire(s.toJson()), j);
  });

  test('Item round-trips and decodes its body', () {
    final j = itemJson(kind: 'Question', status: 'Pending', body: questionBody);
    final i = Item.fromJson(wire(j) as Json);
    expect(i.kind, ItemKind.question);
    expect(i.status, ItemStatus.pending);
    final q = i.question!;
    expect(q.requestId, 'toolu_01ABC');
    expect(q.options, ['yes, fold it', 'no, keep two']);
    expect(q.freeText, isTrue);
    expect(q.answered, isNull);
    expect(wire(i.toJson()), j);
  });

  test('Changes with every row kind round-trips', () {
    final j = changes(
      {'Session': sid},
      52,
      [
        {'Item': itemJson()},
        {
          'Tombstone': {'entity': 'Item', 'id': '$sid/msg_00', 'rev': 51},
        },
      ],
    );
    final c = S2C.fromJson(wire(j) as Object) as Changes;
    expect(c.scope, const Scope.session(sid));
    expect(c.rows.first, isA<ItemRow>());
    expect((c.rows[1] as Tombstone).entity, Entity.item);
    expect(wire(c.toJson()), j);

    final g = changes('Global', 7, [
      {'Session': sessionJson()},
      {
        'Pane': {
          'id': {'sid': sid, 'no': 0},
          'rev': 6,
          'kind': 'Agent',
          'title': 'claude',
        },
      },
      {
        'Issue': {
          'id': 12,
          'rev': 5,
          'repo': 'femme',
          'title': 'phone app',
          'body': '',
          'status': 'Pr',
          'pr_url': null,
          'note': null,
          'session': sid,
          'resume_at': null,
          'created': 1,
          'updated': 2,
        },
      },
    ], reset: true);
    final gc = S2C.fromJson(wire(g) as Object) as Changes;
    expect(gc.scope.isGlobal, isTrue);
    expect(gc.reset, isTrue);
    expect((gc.rows[2] as IssueRow).issue.status, IssueStatus.pr);
    expect(wire(gc.toJson()), g);
  });

  test('Resp and errors', () {
    final ok = RespMsg('X', {
      'Ok': {
        'ServerInfo': {'version': '0.9.1', 'deployed': 'df40bc9'},
      },
    }).unwrap();
    expect(ok.serverInfo.deployed, 'df40bc9');
    expect(RespMsg('X', {'Ok': 'Ok'}).unwrap().isOk, isTrue);
    expect(
      () => RespMsg('X', {
        'Err': {'code': 'NotFound', 'msg': 'no such session'},
      }).unwrap(),
      throwsA(isA<FemmeError>().having((e) => e.code, 'code', 'NotFound')),
    );
  });

  test('Req shapes', () {
    expect(Req.subscribe(Scope.global), {
      'Subscribe': {'scope': 'Global', 'after': null},
    });
    expect(Req.prompt(sid, 'hi'), {
      'Prompt': {'sid': sid, 'text': 'hi', 'attachments': [], 'how': 'Queue'},
    });
    expect(Req.answerQuestion(sid, 'r1', Answer.option(1)), {
      'AnswerQuestion': {
        'sid': sid,
        'request_id': 'r1',
        'answer': {'Option': 1},
      },
    });
    expect(
      (Req.answerQuestion(sid, 'r', Answer.cancel) as Map)['AnswerQuestion'],
      containsPair('answer', 'Cancel'),
    );
    expect(Req.serverInfo, 'ServerInfo');
    expect(Req.transcriptPage(sid, 120), {
      'TranscriptPage': {'sid': sid, 'before_seq': 120, 'limit': 60},
    });
    expect(
      (Req.prompt(sid, 'go', how: PromptHow.interruptThenSend)
          as Map)['Prompt'],
      containsPair('how', 'InterruptThenSend'),
    );
  });

  test('Term frames and keys', () {
    expect(TermUp.key(1, KeyCode.char('a')), {
      'Key': {
        'seq': 1,
        'key': {
          'code': {'Char': 'a'},
          'mods': '',
        },
      },
    });
    expect(
      (TermUp.key(2, KeyCode.enter, mods: {Mods.ctrl, Mods.shift})
          as Map)['Key']['key']['mods'],
      'CTRL | SHIFT',
    );
    final d = TermDown.fromJson(
      wire({
        'Frame': {
          'epoch': 3,
          'rev': 9,
          'base_rev': 0,
          'applied_seq': 2,
          'echo_seq': 2,
          'screen': 'Primary',
          'cols': 80,
          'rows': 24,
          'top_abs': 100,
          'history_lo_abs': 0,
          'cursor': {'row': 1, 'col': 4, 'shape': 'Bar', 'visible': true},
          'modes': 'APP_CURSOR | BRACKETED_PASTE',
          'title': 'claude',
          'new_styles': [
            [
              1,
              {
                'fg': {'Indexed': 2},
                'bg': {
                  'Rgb': [30, 30, 30],
                },
                'underline': 'Default',
                'attrs': 'BOLD',
              },
            ],
          ],
          'row_patches': [
            {
              'abs': 100,
              'wrapped': false,
              'runs': [
                [0, '\$ '],
                [1, 'cargo test'],
              ],
              'wide_cols': [],
            },
          ],
          'timing': null,
        },
      })!,
    ) as FrameDown;
    final f = d.frame;
    expect(f.isSnapshot, isTrue);
    expect(f.modes, {Modes.appCursor, Modes.bracketedPaste});
    expect(f.cursor.shape, CursorShape.bar);
    expect(f.newStyles.single.$2.bg.rgb, (30, 30, 30));
    expect(f.newStyles.single.$2.attrs, {Attrs.bold});
    expect(f.rowPatches.single.text, '\$ cargo test');
    expect(TermDown.fromJson('Bell'), isA<Bell>());
    expect(
      (TermDown.fromJson({
        'Exited': {'status': 1},
      }) as Exited).status,
      1,
    );
  });

  test('ULIDs', () {
    final a = Ulid.next(DateTime.fromMillisecondsSinceEpoch(1794300000000));
    expect(a.length, 26);
    expect(Ulid.isValid(a), isTrue);
    expect(Ulid.timeOf(a), 1794300000000);
    expect(Ulid.next(), isNot(Ulid.next()));
    expect(randomClientInstance(), lessThan(1 << 52));
  });
}
