import 'package:femme_mobile/ui/new_chat.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('split: the folder to list and the filter after the last slash', () {
    expect(splitFolderPath('~/fe'), ('~/', 'fe'));
    expect(splitFolderPath('~/femme/'), ('~/femme/', ''));
    expect(splitFolderPath('~'), ('~', ''));
    expect(splitFolderPath('/srv/x'), ('/srv/', 'x'));
    expect(splitFolderPath('fe'), ('', 'fe'));
  });

  const names = [
    'Documents',
    'docs',
    '.config',
    '.cache',
    'femme',
    'Downloads',
  ];

  test('matching: prefix, any case', () {
    expect(matchingFolders(names, 'do'), ['Documents', 'docs', 'Downloads']);
    expect(matchingFolders(names, 'DOC'), ['Documents', 'docs']);
    expect(matchingFolders(names, 'x'), isEmpty);
  });

  test('matching: hidden folders only when the filter starts with a dot', () {
    expect(matchingFolders(names, ''), [
      'Documents',
      'docs',
      'femme',
      'Downloads',
    ]);
    expect(matchingFolders(names, '.'), ['.config', '.cache']);
    expect(matchingFolders(names, '.CA'), ['.cache']);
  });
}
