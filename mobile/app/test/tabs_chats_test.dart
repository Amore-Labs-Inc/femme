import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/ui/chats.dart';
import 'package:femme_mobile/ui/widgets.dart' show homeShort;
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

Session s(
  String id, {
  required String cwd,
  required int created,
  String status = 'Idle',
  int? closed,
  String? label,
  String? model = 'claude-opus-5-5',
  String agent = 'Claude',
}) => Session.fromJson({
  ...sessionJson(id: id, status: status, label: label),
  'cwd': cwd,
  'created': created,
  'closed': closed,
  'model': model,
  'agent': agent,
});

void main() {
  test('live sessions grouped by folder, project of the newest first', () {
    final all = [
      s('A', cwd: '/home/cem/femme', created: 100),
      s('B', cwd: '/home/cem/Platform/', created: 300),
      s('C', cwd: '/home/cem/femme', created: 200),
      s('D', cwd: '/home/cem', created: 50),
      s('E', cwd: '/home/cem/femme', created: 400, closed: 401),
      s('F', cwd: '/home/cem/femme', created: 500, status: 'Closed'),
    ];
    final p = groupByProject(all);
    expect(p.map((x) => x.dir), [
      '/home/cem/Platform',
      '/home/cem/femme',
      '/home/cem',
    ]);
    expect(p.map((x) => x.header), ['Platform', 'femme', '~']);
    expect(p[1].sessions.map((x) => x.id), ['C', 'A']);
  });

  test('two folders with the same name show their whole path', () {
    final p = groupByProject([
      s('A', cwd: '/home/cem/femme', created: 2),
      s('B', cwd: '/home/cem/work/femme', created: 1),
      s('C', cwd: '/home/cem/Platform', created: 3),
    ]);
    expect(p.map((x) => x.header), ['Platform', '~/femme', '~/work/femme']);
  });

  test('equal created times sort by id, newest id first', () {
    final p = groupByProject([
      s('A1', cwd: '/x', created: 1),
      s('A2', cwd: '/x', created: 1),
    ]);
    expect(p.single.sessions.map((x) => x.id), ['A2', 'A1']);
  });

  test('search keeps sessions whose title or folder match, any case', () {
    final all = [
      s('A', cwd: '/home/cem/femme', created: 1, label: 'Lesson two'),
      s('B', cwd: '/home/cem/Platform', created: 2, label: 'hero'),
    ];
    expect(groupByProject(all, query: 'LESSON').single.sessions.single.id, 'A');
    expect(
      groupByProject(all, query: 'platform').single.sessions.single.id,
      'B',
    );
    expect(groupByProject(all, query: 'nothing'), isEmpty);
  });

  test('second line: agent or gateway provider, and the age', () {
    final now = DateTime.fromMillisecondsSinceEpoch(3 * 3600 * 1000 + 10);
    expect(
      sessionSubtitle(s('A', cwd: '/x', created: 10), now: now),
      'claude · 3h',
    );
    expect(
      sessionSubtitle(
        s('B', cwd: '/x', created: 10, model: 'deepseek/deepseek-v4.1'),
        now: now,
      ),
      'deepseek · 3h',
    );
    expect(
      sessionSubtitle(
        s('C', cwd: '/x', created: 10, model: null, agent: 'Shell'),
        now: now,
      ),
      'shell · 3h',
    );
  });

  test('home shortening', () {
    expect(homeShort('/home/cem'), '~');
    expect(homeShort('/home/cem/femme/'), '~/femme');
    expect(homeShort('/Users/ada/x'), '~/x');
    expect(homeShort('/root'), '~');
    expect(homeShort('/srv/data'), '/srv/data');
  });
}
