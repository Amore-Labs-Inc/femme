import 'dart:io';

import 'package:femme_mobile/ui/media/poster.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:video_player/video_player.dart';

final bytes = Uint8List.fromList([1, 2, 3]);

/// A cache whose "open" makes an uninitialised controller (no platform call) and counts.
(PosterCache, List<String>, List<VideoPlayerController>) fakeCache({
  int cap = 3,
}) {
  final opened = <String>[];
  final closed = <VideoPlayerController>[];
  final cache = PosterCache(
    cap: cap,
    supported: true,
    open: (b, name) async {
      opened.add(name);
      return VideoPlayerController.file(File('/nonexistent/$name'));
    },
    close: closed.add,
  );
  return (cache, opened, closed);
}

void main() {
  test('a held key is not opened again and moves to the newest end', () async {
    final (cache, opened, _) = fakeCache();
    final a = await cache.acquire('a', bytes, 'a.mp4');
    cache.release('a');
    await cache.acquire('b', bytes, 'b.mp4');
    cache.release('b');
    expect(await cache.acquire('a', bytes, 'a.mp4'), same(a));
    cache.release('a');
    expect(opened, ['a.mp4', 'b.mp4']);
    expect(cache.keys, ['b', 'a']);
  });

  test('past the cap the least recently used unused one is closed', () async {
    final (cache, _, closed) = fakeCache(cap: 2);
    final a = await cache.acquire('a', bytes, 'a.mp4');
    cache.release('a');
    await cache.acquire('b', bytes, 'b.mp4');
    cache.release('b');
    await cache.acquire('c', bytes, 'c.mp4');
    cache.release('c');
    await pumpEventQueue();
    expect(cache.keys, ['b', 'c']);
    expect(closed, [a]);
  });

  test('one still on screen is kept even past the cap', () async {
    final (cache, _, closed) = fakeCache(cap: 1);
    await cache.acquire('a', bytes, 'a.mp4'); // still shown
    await cache.acquire('b', bytes, 'b.mp4'); // still shown
    await pumpEventQueue();
    expect(cache.keys, ['a', 'b']);
    expect(closed, isEmpty);
    cache.release('a');
    await pumpEventQueue();
    expect(cache.keys, ['b']);
    expect(closed, hasLength(1));
  });

  test(
    'a failed open (no plugin, bad codec) is null and not retried',
    () async {
      var tries = 0;
      final cache = PosterCache(
        supported: true,
        open: (_, _) async {
          tries++;
          throw MissingPluginException('no video_player here');
        },
      );
      expect(await cache.acquire('a', bytes, 'a.mp4'), isNull);
      cache.release('a');
      expect(await cache.acquire('a', bytes, 'a.mp4'), isNull);
      expect(tries, 1);
    },
  );

  test('unsupported platform opens nothing', () async {
    var tries = 0;
    final cache = PosterCache(
      supported: false,
      open: (_, _) async {
        tries++;
        throw StateError('unreachable');
      },
    );
    expect(await cache.acquire('a', bytes, 'a.mp4'), isNull);
    expect(tries, 0);
    expect(cache.keys, isEmpty);
  });

  test('Linux desktop (where tests run) has no poster by default', () {
    expect(PosterCache().supported, isFalse);
  });

  testWidgets('the poster shows the fallback when the player fails', (
    tester,
  ) async {
    final cache = PosterCache(
      supported: true,
      open: (_, _) async => throw PlatformException(code: 'codec'),
    );
    await tester.pumpWidget(
      Directionality(
        textDirection: TextDirection.ltr,
        child: VideoPoster(
          blobKey: 's\na.mp4',
          bytes: bytes,
          name: 'a.mp4',
          cache: cache,
          fallback: const Text('plain tile'),
        ),
      ),
    );
    await tester.pump();
    expect(find.text('plain tile'), findsOneWidget);
    expect(find.byType(VideoPlayer), findsNothing);
    await tester.pumpWidget(const SizedBox());
    expect(cache.keys, ['s\na.mp4']); // released, kept for a later scroll back
  });
}
