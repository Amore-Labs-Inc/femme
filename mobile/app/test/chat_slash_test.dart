import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/ui/chat/slash.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  const names = [
    'compact',
    'clear',
    'model',
    'effort',
    'goal',
    'btw',
    'context',
    'cost',
    'mcp',
    'memory',
    'login',
    'logout',
    'color',
  ];

  test('prefix matches alphabetically, then substring matches', () {
    expect(matchSlash('/co', names), ['color', 'compact', 'context', 'cost']);
    expect(matchSlash('/o', names), [
      'color',
      'compact',
      'context',
      'cost',
      'effort',
      'goal',
      'login',
      'logout',
    ]);
    expect(matchSlash('/c', names).take(2), ['clear', 'color']);
  });

  test('at most 8; nothing once a space is typed or without /', () {
    expect(matchSlash('/', names).length, 8);
    expect(matchSlash('/model ', names), isEmpty);
    expect(matchSlash('model', names), isEmpty);
    expect(matchSlash('/', ['/a', 'b']), ['a', 'b']);
  });

  test('/model picker: models, then efforts, then the prompts', () {
    final m = modelStep('/model ');
    expect(m, isA<PickModel>());
    expect((m as PickModel).choices.map((c) => c.name), [
      'opus',
      'sonnet',
      'haiku',
      'fable',
      'deepseek',
    ]);
    final e = modelStep('/model opus ');
    expect((e as PickEffort).choices, [
      'low',
      'medium',
      'high',
      'xhigh',
      'max',
    ]);
    expect((modelStep('/model opus z') as PickEffort?), isNull);
    expect(modelStep('/model deepseek '), isNull);
    expect(modelPrompts(e.model, 'high'), [
      '/model claude-opus-5-5',
      '/effort high',
    ]);
    expect(modelPrompts(models.last, null), [
      '/model deepseek/deepseek-v4.1-flash-ultrafast',
    ]);
  });

  test('model labels', () {
    expect(modelLabel('claude-opus-5-5'), 'opus 5.5');
    expect(modelLabel('claude-opus-5-5[1m]'), 'opus 5.5');
    expect(modelLabel('openrouter/claude-sonnet-5-5'), 'sonnet 5.5');
    expect(modelLabel('claude-haiku-4-5-20251001'), 'haiku 4.5');
    expect(
      modelLabel('deepseek/deepseek-v4.1-flash-ultrafast'),
      'deepseek-v4.1-flash-ultrafast',
    );
    expect(modelLabel(null), '');
  });

  test('durations', () {
    expect(duration(5000), '5s');
    expect(duration(180000), '3m');
    expect(duration(192000), '3m 12s');
    expect(duration(3840000), '1h 4m');
  });
}
