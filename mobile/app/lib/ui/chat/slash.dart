/// The `/` menu (PARITY §7): matching the daemon's commands, and the client-side `/model`
/// picker that sends `/model <id>` then `/effort <e>`.
library;

/// The command names matching [text] while it is `/word` with no space: prefix matches in
/// alphabetical order, then substring matches, at most [max]. Empty when [text] is not a
/// command being typed.
List<String> matchSlash(String text, Iterable<String> names, {int max = 8}) {
  if (!text.startsWith('/') || text.contains(RegExp(r'\s'))) return const [];
  final w = text.substring(1).toLowerCase();
  final all = {
    for (final n in names) n.startsWith('/') ? n.substring(1) : n,
  }.toList()..sort();
  final prefix = [
    for (final n in all)
      if (n.toLowerCase().startsWith(w)) n,
  ];
  final sub = [
    for (final n in all)
      if (!n.toLowerCase().startsWith(w) && n.toLowerCase().contains(w)) n,
  ];
  return [...prefix, ...sub].take(max).toList();
}

class ModelChoice {
  const ModelChoice(this.name, this.id, this.label, {this.claude = true});
  final String name;
  final String id;
  final String label;

  /// A Claude model takes an effort next; others switch at once.
  final bool claude;
}

const models = [
  ModelChoice('opus', 'claude-opus-5-5', 'opus 5.5'),
  ModelChoice('sonnet', 'claude-sonnet-5-5', 'sonnet 5.5'),
  ModelChoice('haiku', 'claude-haiku-5-5', 'haiku 5.5'),
  ModelChoice('fable', 'claude-fable-5-2', 'fable 5.2'),
  ModelChoice(
    'deepseek',
    'deepseek/deepseek-v4.1-flash-ultrafast',
    'deepseek v4.1 flash',
    claude: false,
  ),
];

const efforts = ['low', 'medium', 'high', 'xhigh', 'max'];

/// What the `/model` picker shows for [text]: models after `/model `, efforts after
/// `/model <claude model> `; null when the text is not in the picker.
sealed class ModelStep {
  const ModelStep();
}

class PickModel extends ModelStep {
  const PickModel(this.choices);
  final List<ModelChoice> choices;
}

class PickEffort extends ModelStep {
  const PickEffort(this.model, this.choices);
  final ModelChoice model;
  final List<String> choices;
}

ModelStep? modelStep(String text) {
  const head = '/model ';
  if (!text.startsWith(head)) return null;
  final rest = text.substring(head.length);
  final parts = rest.split(' ');
  if (parts.length == 1) {
    final w = parts[0].toLowerCase();
    final m = [
      for (final c in models)
        if (c.name.startsWith(w)) c,
    ];
    return m.isEmpty ? null : PickModel(m);
  }
  if (parts.length == 2) {
    final model = models.where((c) => c.name == parts[0] && c.claude);
    if (model.isEmpty) return null;
    final w = parts[1].toLowerCase();
    final e = [
      for (final x in efforts)
        if (x.startsWith(w)) x,
    ];
    return e.isEmpty ? null : PickEffort(model.first, e);
  }
  return null;
}

/// The prompts a finished `/model` pick sends, in order.
List<String> modelPrompts(ModelChoice m, String? effort) => [
  '/model ${m.id}',
  if (m.claude && effort != null) '/effort $effort',
];
