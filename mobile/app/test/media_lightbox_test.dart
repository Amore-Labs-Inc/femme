import 'dart:typed_data';

import 'package:femme_mobile/ui/media/lightbox.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:video_player/video_player.dart';

/// A player that never reaches the platform: initialised at once, records volumes.
class FakePlayer extends VideoPlayerController {
  FakePlayer(super.file, this.volumes) : super.file();
  final List<double> volumes;

  @override
  Future<void> initialize() async {
    value = value.copyWith(
      isInitialized: true,
      duration: const Duration(seconds: 10),
      size: const Size(160, 90),
    );
  }

  @override
  Future<void> play() async => value = value.copyWith(isPlaying: true);

  @override
  Future<void> pause() async => value = value.copyWith(isPlaying: false);

  @override
  Future<void> setVolume(double volume) async {
    volumes.add(volume);
    value = value.copyWith(volume: volume);
  }

  /// The platform player it would dispose was never made.
  @override
  // ignore: must_call_super
  Future<void> dispose() async {}
}

/// Opens the lightbox and waits (real time, for the temp file) until the player is made.
Future<void> open(WidgetTester tester, List<double> volumes) async {
  await tester.pumpWidget(
    CupertinoApp(
      home: Builder(
        builder: (context) => CupertinoButton(
          onPressed: () => showLightbox(
            context,
            bytes: Uint8List.fromList([0, 0, 0]),
            name: 'clip.mp4',
            video: true,
          ),
          child: const Text('open'),
        ),
      ),
    ),
  );
  await tester.tap(find.text('open'));
  for (var i = 0; i < 50 && volumes.isEmpty; i++) {
    await tester.runAsync(
      () => Future.delayed(const Duration(milliseconds: 30)),
    );
    await tester.pump(const Duration(milliseconds: 200));
  }
}

void main() {
  testWidgets(
    'the video sound toggle mutes, and the choice holds for the run',
    (tester) async {
      lightboxMuted = false;
      final volumes = <double>[];
      lightboxPlayer = (f) => FakePlayer(f, volumes);

      await open(tester, volumes);
      expect(volumes, [1.0]);
      expect(find.byIcon(CupertinoIcons.speaker_2_fill), findsOneWidget);
      await tester.tap(find.byIcon(CupertinoIcons.speaker_2_fill));
      await tester.pump();
      expect(volumes.last, 0.0);
      expect(find.byIcon(CupertinoIcons.speaker_slash_fill), findsOneWidget);

      // reopened: starts muted
      await tester.pumpWidget(const SizedBox());
      volumes.clear();
      await open(tester, volumes);
      expect(volumes, [0.0]);
      expect(find.byIcon(CupertinoIcons.speaker_slash_fill), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      await tester.runAsync(
        () => Future.delayed(const Duration(milliseconds: 30)),
      );
      lightboxMuted = false;
    },
  );
}
