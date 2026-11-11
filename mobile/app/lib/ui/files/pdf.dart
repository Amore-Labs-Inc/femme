import 'package:flutter/cupertino.dart';
import 'package:flutter/foundation.dart';
import 'package:pdfrx/pdfrx.dart';

import '../theme.dart';

/// `3 / 12`: the page shown and how many there are.
String pageLabel(int page, int count) => '$page / $count';

/// A PDF from bytes, one page at a time: swipe sideways for the next, pinch to zoom, the page
/// number under it (desktop `viewer/pdf.rs` draws the pages in a column; a phone pages).
class PdfPages extends StatefulWidget {
  const PdfPages({super.key, required this.name, required this.bytes});

  final String name;
  final Uint8List bytes;

  @override
  State<PdfPages> createState() => _PdfPagesState();
}

class _PdfPagesState extends State<PdfPages> {
  final _pages = PageController();
  int _page = 1;

  @override
  void dispose() {
    _pages.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    return ColoredBox(
      color: FemmeColors.panelC.resolveFrom(context),
      child: PdfDocumentViewBuilder(
        documentRef: PdfDocumentRefData(
          widget.bytes,
          sourceName: 'femme-files:${widget.name}:${widget.bytes.length}',
        ),
        loadingBuilder: (_) =>
            const Center(child: CupertinoActivityIndicator()),
        errorBuilder: (context, error, _) => Center(
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Text(
              'This PDF could not be opened.\n$error',
              textAlign: TextAlign.center,
              style: TextStyle(color: muted),
            ),
          ),
        ),
        builder: (context, doc) {
          if (doc == null) {
            return const Center(child: CupertinoActivityIndicator());
          }
          final count = doc.pages.length;
          return Column(
            children: [
              Expanded(
                child: PageView.builder(
                  controller: _pages,
                  itemCount: count,
                  onPageChanged: (i) => setState(() => _page = i + 1),
                  itemBuilder: (context, i) => InteractiveViewer(
                    maxScale: 6,
                    child: Padding(
                      padding: const EdgeInsets.all(12),
                      child: PdfPageView(
                        document: doc,
                        pageNumber: i + 1,
                        backgroundColor: const Color(0xFFFFFFFF),
                      ),
                    ),
                  ),
                ),
              ),
              Padding(
                padding: EdgeInsets.only(
                  top: 6,
                  bottom: 10 + MediaQuery.paddingOf(context).bottom,
                ),
                child: Text(
                  pageLabel(_page, count),
                  style: TextStyle(
                    color: muted,
                    fontSize: 13,
                    fontFeatures: const [FontFeature.tabularFigures()],
                  ),
                ),
              ),
            ],
          );
        },
      ),
    );
  }
}
