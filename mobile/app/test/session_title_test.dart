import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

Session s({String? name, String? label, String cwd = '/home/cem/femme'}) =>
    Session.fromJson(sessionJson(name: name, label: label, cwd: cwd));

void main() {
  test('title: name before label', () {
    expect(s(name: 'fix login', label: 'lovable').title, 'fix login');
  });

  test('title: label when no name, empty name counts as none', () {
    expect(s(label: 'lovable').title, 'lovable');
    expect(s(name: '', label: 'lovable').title, 'lovable');
  });

  test('title: last folder of cwd, trailing slash ignored', () {
    expect(s(label: null).title, 'femme');
    expect(s(label: '', cwd: '/home/cem/femme/').title, 'femme');
  });

  test('title: handle when cwd has no last component', () {
    expect(s(label: null, cwd: '/').title, 'k3');
    expect(s(label: null, cwd: '').title, 'k3');
  });

  test('title: lists and chat header agree', () {
    for (final x in [
      s(name: 'n', label: 'l'),
      s(label: 'l'),
      s(label: null),
      s(label: null, cwd: '/'),
    ]) {
      expect(x.title, sessionTitle(x));
    }
  });
}
