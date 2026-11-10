import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/ui/chat/attached.dart';
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
}
