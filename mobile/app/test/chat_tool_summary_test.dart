import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/ui/chat/tool_summary.dart';
import 'package:flutter_test/flutter_test.dart';

import 'chat_items.dart';

void main() {
  // BODIES.md §4.2: tool, input, the desktop's line.
  final table = <(String, Map<String, Object?>, String, String)>[
    (
      'Bash',
      {'command': 'echo hi', 'description': 'Print hi'},
      'Bash',
      'echo hi',
    ),
    ('Bash', {'command': '\n  cargo test\nmore'}, 'Bash', 'cargo test'),
    ('Read', {'file_path': '/etc/hostname'}, 'Read', '/etc/hostname'),
    ('Write', {'file_path': '/a/b.txt', 'content': 'x'}, 'Write', '/a/b.txt'),
    (
      'Edit',
      {'file_path': '/a', 'old_string': 'x', 'new_string': 'y'},
      'Edit',
      '/a',
    ),
    ('NotebookEdit', {'notebook_path': '/n.ipynb'}, 'NotebookEdit', ''),
    ('Grep', {'pattern': 'foo', 'path': 'src'}, 'Grep', 'src'),
    ('Grep', {'pattern': 'foo'}, 'Grep', 'foo'),
    ('Glob', {'pattern': '**/*.rs'}, 'Glob', '**/*.rs'),
    (
      'WebFetch',
      {'url': 'https://x.dev', 'prompt': 'p'},
      'WebFetch',
      'https://x.dev',
    ),
    ('WebSearch', {'query': 'flutter'}, 'WebSearch', 'flutter'),
    (
      'ToolSearch',
      {'query': 'select:ExitPlanMode', 'max_results': 1},
      'ToolSearch',
      'select:ExitPlanMode',
    ),
    (
      'Agent',
      {'description': 'find bugs', 'prompt': 'long'},
      'Agent',
      'find bugs',
    ),
    ('TodoWrite', {'todos': []}, 'TodoWrite', ''),
    ('EnterPlanMode', {}, 'EnterPlanMode', ''),
    ('mcp__elicit__ask_form', {'x': 1}, 'mcp__elicit__ask_form', ''),
    ('Bash', {'command': 42, 'description': 'num'}, 'Bash', 'num'),
  ];
  for (final (tool, input, name, hint) in table) {
    test('$tool $input → $name $hint', () {
      final c = call('t', tool, input);
      final l = toolLine(call: c, callBody: c.bodyJson);
      expect(l.name, name);
      expect(l.hint, hint);
      expect(l.result, isNull);
    });
  }

  test('result: first non-empty line; failed is red', () {
    final c = call('t1', 'Bash', {'command': 'x'});
    final r = result('t1', '\n\nhi\nthere', error: true);
    final l = toolLine(
      call: c,
      callBody: c.bodyJson,
      result: r,
      resultBody: r.bodyJson,
    );
    expect(l.result, 'hi');
    expect(l.failed, isTrue);
  });

  test('no body at hand: name and hint from the preview', () {
    final c = it(
      't2',
      ItemKind.toolCall,
      status: ItemStatus.inProgress,
      preview: 'Bash ls -la\nmore',
    );
    final l = toolLine(call: c);
    expect(l.name, 'Bash');
    expect(l.hint, 'ls -la');
    expect(l.running, isTrue);
  });

  test('cut body: the name is salvaged', () {
    final c = it(
      't3',
      ItemKind.toolCall,
      preview: 'x',
      body: '{"call_id":"t3","input":{"command":"a"},"name":"Bash","ta…[truncated]',
    );
    expect(toolLine(call: c).name, 'Bash');
  });

  test('result body not fetched: the preview is its first line', () {
    final c = call('t4', 'Read', {'file_path': '/x'});
    final r = it('t4:result', ItemKind.toolResult, preview: 'line one');
    expect(
      toolLine(call: c, callBody: c.bodyJson, result: r).result,
      'line one',
    );
  });

  test(
    'foreground task running → backgroundable; background until its event',
    () {
      final fg = call(
        't5',
        'Bash',
        {'command': 'sleep 45'},
        status: ItemStatus.inProgress,
        task: 'foreground',
      );
      final l = toolLine(call: fg, callBody: fg.bodyJson);
      expect(l.backgroundable, isTrue);
      final bg = call('t6', 'Bash', {'command': 'sleep'}, task: 'background');
      expect(toolLine(call: bg, callBody: bg.bodyJson).inBackground, isTrue);
      expect(
        toolLine(
          call: bg,
          callBody: bg.bodyJson,
          endedTasks: {'t6'},
        ).inBackground,
        isFalse,
      );
    },
  );

  test('Artifact with a link', () {
    final c = call('t7', 'Artifact', {'file_path': '/tmp/p.html'});
    final r = result(
      't7',
      'Published',
      link: {
        'title': 'Page',
        'updated': true,
        'url': 'https://claude.ai/artifact/A',
        'version': 3,
      },
    );
    final l = toolLine(
      call: c,
      callBody: c.bodyJson,
      result: r,
      resultBody: r.bodyJson,
    );
    expect(l.name, 'artifact');
    expect(l.hint, '· Page ↗');
    expect(l.result, 'updated · v3');
    expect(l.linkUrl, 'https://claude.ai/artifact/A');
  });

  test('PushNotification: one line, with what femmed did with it', () {
    final input = {'message': 'tests green\nall 12', 'status': 'proactive'};
    final c = call('t8', 'PushNotification', input);
    final r = result('t8', 'Mobile push not sent (Remote Control inactive).');
    ToolLine line(String? push) => toolLine(
      call: c,
      callBody: {...?c.bodyJson, 'push': ?push},
      result: r,
      resultBody: r.bodyJson,
    );
    expect(line('sent').name, '🔔 push');
    expect(line('sent').hint, '· tests green');
    expect(line('sent').result, 'sent to your phone');
    expect(line('no phone registered').result, 'not sent: no phone registered');
    // femmed left it alone: Claude Code's answer
    expect(line(null).result, startsWith('Mobile push not sent'));
  });

  test('opened input: bare command, pretty JSON, nothing for {}', () {
    expect(inputText({'command': 'ls'}), 'ls');
    expect(inputText({}), '');
    expect(inputText({'a': 1}), '{\n  "a": 1\n}');
  });

  test('cap at 400 lines', () {
    final s = List.generate(500, (i) => '$i').join('\n');
    final c = capped(s);
    expect(c.split('\n').length, 401);
    expect(c.endsWith('… (cut)'), isTrue);
  });

  test('run sentence', () {
    expect(
      runSentence([
        'Bash',
        'Bash',
        'Read',
        'Grep',
        'Edit',
        'Write',
        'WebFetch',
        'Foo',
      ]),
      'Ran 2 commands, read 1 file, searched 1 time, edited 2 files, fetched 1 page, 1 Foo call',
    );
  });
}
