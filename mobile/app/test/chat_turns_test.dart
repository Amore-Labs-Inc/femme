import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/wire.dart';
import 'package:femme_mobile/ui/chat/attached.dart';
import 'package:femme_mobile/ui/chat/rows.dart';
import 'package:femme_mobile/ui/chat/turns.dart';
import 'package:flutter_test/flutter_test.dart';

import 'chat_items.dart';

List<Entry> lay(List<Item> items) => layout(items, body: (i) => i.bodyJson);

void main() {
  test('call and result are one row; an orphan result is its own row', () {
    final rows = lay([
      call('a', 'Bash', {'command': 'ls'}),
      result('a', 'x'),
      result('zz', 'orphan'),
    ]);
    expect(rows, hasLength(2));
    expect((rows[0] as ToolEntry).result?.itemId, 'a:result');
    expect((rows[1] as ToolEntry).call, isNull);
  });

  test('a finished turn folds its thinking and tools into one line', () {
    final rows = lay([
      it('u', ItemKind.userMessage, body: {'text': 'go'}),
      it('th', ItemKind.thinking, body: {'text': 'hmm'}),
      call('a', 'Bash', {'command': 'ls'}),
      result('a', 'x'),
      it('m1', ItemKind.assistantMessage, body: {'text': 'mid'}),
      call('b', 'Read', {'file_path': '/f'}, status: ItemStatus.failed),
      result('b', 'nope', error: true),
      it('m2', ItemKind.assistantMessage, body: {'text': 'done'}),
      it(
        'end',
        ItemKind.turnEnd,
        body: {'duration_ms': 192000, 'stop': 'EndTurn', 'usage': null},
      ),
    ]);
    expect(rows.map((e) => e.runtimeType.toString()), [
      'ItemEntry',
      'FoldEntry',
      'ItemEntry',
      'ItemEntry',
    ]);
    final f = rows[1] as FoldEntry;
    expect(f.label, 'worked for 3m 12s · 2 tools · 1 failed');
    expect(f.inner, hasLength(3));
  });

  test('a turn of only thinking reads "thought for"', () {
    final rows = lay([
      it('th', ItemKind.thinking, body: {'text': 'hmm'}, ts: 1000),
      it('m', ItemKind.assistantMessage, body: {'text': 'ok'}, ts: 3000),
      it(
        'end',
        ItemKind.turnEnd,
        body: {'duration_ms': 5000, 'stop': 'EndTurn'},
      ),
    ]);
    expect((rows[0] as FoldEntry).label, 'thought for 5s');
  });

  test(
    'the running turn folds runs of calls but the last; running calls stay',
    () {
      final rows = lay([
        it('u', ItemKind.userMessage, body: {'text': 'go'}),
        call('a', 'Bash', {'command': '1'}),
        result('a', ''),
        call('b', 'Bash', {'command': '2'}),
        result('b', ''),
        call('c', 'Read', {'file_path': '/x'}),
        result('c', ''),
        call('d', 'Grep', {'pattern': 'p'}),
        result('d', ''),
        call(
          'e',
          'Bash',
          {'command': 'sleep'},
          status: ItemStatus.inProgress,
          task: 'foreground',
        ),
      ]);
      expect(rows.map((e) => e.runtimeType.toString()), [
        'ItemEntry',
        'FoldEntry',
        'ToolEntry',
        'ToolEntry',
      ]);
      expect((rows[1] as FoldEntry).label, 'Ran 2 commands, read 1 file');
      expect(rows[2].key, 'd');
      expect(rows[3].key, 'e');
    },
  );

  test('notable turn ends; EndTurn has no row', () {
    expect(turnEndNote({'stop': 'EndTurn'}), isNull);
    expect(turnEndNote({'stop': 'Interrupted'})!.text, '· interrupted');
    expect(
      turnEndNote({'stop': 'MaxTokens'})!.text,
      '· stopped: output limit reached',
    );
    expect(
      turnEndNote({
        'stop': {'Error': 'boom'},
      }),
      (text: '· turn failed: boom', red: true),
    );
    expect(
      turnEndNote({
        'stop': {'Other': 'x'},
      })!.text,
      '· turn ended: x',
    );
    final rows = lay([
      it('m', ItemKind.assistantMessage, body: {'text': 'a'}),
      it('end', ItemKind.turnEnd, body: {'stop': 'Interrupted'}),
    ]);
    expect(rows.last, isA<TurnEndEntry>());
  });

  test('a read mark right below its message is hidden, elsewhere shown', () {
    final rows = lay([
      it('u1', ItemKind.userMessage, body: {'text': 'one'}),
      it(
        'r1',
        ItemKind.event,
        body: {'read': 'u1', 'text': 'Claude read your message'},
      ),
      it('u2', ItemKind.userMessage, body: {'text': 'two'}),
      it('m', ItemKind.assistantMessage, body: {'text': 'a'}),
      it(
        'r2',
        ItemKind.event,
        body: {'read': 'u2', 'text': 'Claude read your message'},
      ),
    ]);
    expect(rows.whereType<ReadEntry>().map((r) => r.target), ['u2']);
  });

  test('a model fallback is a warning event', () {
    final warn = it(
      'u-f',
      ItemKind.event,
      body: {
        'text': 'fable 5.2 → opus 4.8 · x',
        'warning': true,
        'detail': 'd',
      },
    );
    expect(eventWarning(warn), isTrue);
    expect(
      eventWarning(
        it('e', ItemKind.event, body: {'text': 'Conversation compacted'}),
      ),
      isFalse,
    );
  });

  test('compaction: the summary folds, its event is hidden', () {
    final rows = lay([
      it('ev', ItemKind.event, body: {'text': 'Conversation compacted'}),
      it(
        'u',
        ItemKind.userMessage,
        body: {'text': '$compactionPrefix. Summary: …'},
      ),
    ]);
    expect(rows, hasLength(1));
    expect(rows.single, isA<CompactionEntry>());
  });

  test('background tasks stay out of the fold until their event', () {
    final items = [
      call('a', 'Bash', {'command': 'serve'}, task: 'background'),
      result('a', 'started'),
      it(
        'end',
        ItemKind.turnEnd,
        body: {'stop': 'EndTurn', 'duration_ms': 1000},
      ),
    ];
    expect(lay(items).first, isA<ToolEntry>());
    final ended = endedTasksOf([
      it(
        'ev',
        ItemKind.event,
        body: {
          'task': {'call': 'a', 'status': 'completed'},
          'text': 'serve: completed',
        },
      ),
    ], (i) => i.bodyJson);
    expect(ended, {'a'});
    expect(
      layout(items, body: (i) => i.bodyJson, endedTasks: ended).first,
      isA<FoldEntry>(),
    );
  });

  test('working verb and since', () {
    final items = [
      it('u', ItemKind.userMessage, body: {'text': 'go'}, ts: 42),
      call('a', 'Bash', {'command': 'x'}, status: ItemStatus.inProgress),
    ];
    expect(workingVerb(items), 'Running a tool');
    expect(lastUserTs(items), 42);
  });

  group('subagent chips', () {
    Json? b(Item i) => i.bodyJson;
    Item agent(String id, String desc) =>
        call(id, 'Agent', {'description': desc, 'prompt': '…'});
    Item ended(String id, String callId) => it(
      id,
      ItemKind.event,
      body: {
        'text': 'done: completed',
        'task': {'call': callId, 'status': 'completed'},
      },
    );
    Item turnEnd(String id) =>
        it(id, ItemKind.turnEnd, body: {'stop': 'EndTurn'});
    List<(String, bool)> on(List<AgentChip> c) => [
      for (final x in c) (x.id, x.running),
    ];

    test("the turn's Agent calls are chips until the turn ends", () {
      final c = agentChips([
        agent('t1', 'old'),
        result('t1', 'done'),
        turnEnd('e1'),
        it('u2', ItemKind.userMessage, body: {'text': 'go'}),
        agent('t2', 'bench the strip'),
        result('t2', 'done'),
        // a big prompt: no body at hand, read from the preview
        it('t3', ItemKind.toolCall, preview: 'Agent review the diff'),
        call('t4', 'Bash', {'command': 'ls'}),
      ], b);
      expect(
        [for (final x in c) (x.id, x.label, x.running)],
        [('t2', 'bench the strip', false), ('t3', 'review the diff', true)],
      );
      expect(agentElapsed(5000, 77500), '1m 12s');
      expect(agentElapsed(5000, 9000), '4s');
      expect(agentElapsed(5000, 4000), '0s');
    });

    test('a background agent stays on show until it is said to stop', () {
      final items = [
        agent('b1', 'bench in the background'),
        result('b1', 'Async agent launched successfully'),
        agent('f1', 'in the foreground'),
        it('f1u', ItemKind.userMessage, parent: 'f1'),
        result('f1', 'done'),
        turnEnd('e1'),
        it('u2', ItemKind.userMessage, body: {'text': 'go'}),
        it('b1c', ItemKind.toolCall, preview: 'Bash ls', parent: 'b1'),
      ];
      expect(on(agentChips(items, b)), [('b1', true)]);
      items.add(ended('n9', 'b1'));
      expect(agentChips(items, b), isEmpty);
      items.addAll([
        agent('b2', 'again'),
        result('b2', 'Async agent launched successfully'),
        ended('n12', 'b2'),
      ]);
      expect(on(agentChips(items, b)), [('b2', false)]);
      items.add(turnEnd('e2'));
      expect(agentChips(items, b), isEmpty);
    });

    test(
      'Task is the old name; the subagent type stands in for a description',
      () {
        final c = agentChips([
          call('k1', 'Task', {'subagent_type': 'Explore', 'prompt': 'x'}),
        ], b);
        expect(c.single.label, 'Explore');
      },
    );
  });

  test('the working line names the work, the time and the tokens', () {
    final items = [
      it('u1', ItemKind.userMessage, body: {'text': 'old question'}, ts: 1000),
      it('a1', ItemKind.assistantMessage, body: {'text': 'old, not counted'}),
      it(
        'u2',
        ItemKind.userMessage,
        body: {'text': 'fix the build'},
        ts: 10000,
      ),
      it('t', ItemKind.thinking, body: {'text': 't' * 4000}),
      it(
        'w',
        ItemKind.assistantMessage,
        body: {'text': 'w' * 2000},
        status: ItemStatus.inProgress,
      ),
    ];
    final verb = workingVerb(items);
    final since = lastUserTs(items);
    expect((verb, since), ('Writing', 10000));
    final bytes = turnBytes(items, (i) => i.text);
    expect(bytes, 6000);
    expect(
      workingText(verb, since, bytes, 10000 + 342000),
      '· Writing… (5m 42s · ↓ 1.5k tokens)',
    );
    expect(workingText('Thinking', null, 0, 0), '· Thinking…');
    expect(workingElapsed(3725), '1h 2m');
    expect(tokenCount(3999), '999');
    expect(utf8Length('é✻😀'), 2 + 3 + 4);
  });

  test("a peer's message opens the turn; between items it is thinking", () {
    final items = [
      it('u', ItemKind.userMessage, body: {'text': 'go'}, ts: 1000),
      it('p', ItemKind.peerMessage, body: {'text': 'hi'}, ts: 10000),
      call('a', 'Bash', {'command': 'x'}),
    ];
    expect(lastUserTs(items), 10000);
    expect(workingVerb(items), 'Thinking');
  });
}
