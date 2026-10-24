import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

final class _ErikaHttpHeader extends Struct {
  external Pointer<Utf8> name;
  external Pointer<Utf8> value;
}

final class _ErikaGifExportOptions extends Struct {
  external Pointer<Utf8> inputUri;
  external Pointer<Utf8> outputPath;

  @Uint64()
  external int startMillis;

  @Uint64()
  external int endMillis;

  @Uint32()
  external int framesPerSecond;

  @Uint32()
  external int outputWidth;

  @Uint32()
  external int outputHeight;

  @Int32()
  external int quality;

  @Int32()
  external int loopCount;

  @Bool()
  external bool overwrite;

  external Pointer<_ErikaHttpHeader> headers;

  @IntPtr()
  external int headerCount;

  @Uint64()
  external int httpReadAheadBytes;

  @Array(3)
  external Array<Uint64> reserved;
}

final class _ErikaGifExportResult extends Struct {
  @Uint32()
  external int width;

  @Uint32()
  external int height;

  @Uint64()
  external int frameCount;

  @Uint64()
  external int fileSize;
}

typedef _ExportGifNative = Int32 Function(
  Pointer<_ErikaGifExportOptions>,
  Pointer<_ErikaGifExportResult>,
);
typedef _ExportGifDart = int Function(
  Pointer<_ErikaGifExportOptions>,
  Pointer<_ErikaGifExportResult>,
);
final class _ErikaFrameExportOptions extends Struct {
  external Pointer<Utf8> inputUri;

  external Pointer<_ErikaHttpHeader> headers;

  @IntPtr()
  external int headerCount;

  @Uint64()
  external int positionMillis;

  @Uint64()
  external int httpReadAheadBytes;

  @Uint32()
  external int maxWidth;

  @Uint32()
  external int maxHeight;

  @Int32()
  external int format;

  @Uint32()
  external int reservedFlags;

  @Array(4)
  external Array<Uint64> reserved;
}

final class _ErikaFrameExportResult extends Struct {
  external Pointer<Uint8> data;

  @IntPtr()
  external int dataLen;

  @Uint64()
  external int timestampMicros;

  @Uint64()
  external int durationMicros;

  @Uint32()
  external int width;

  @Uint32()
  external int height;

  @Uint32()
  external int sourceWidth;

  @Uint32()
  external int sourceHeight;

  @Int32()
  external int format;

  @Uint32()
  external int reservedFlags;

  @Array(2)
  external Array<Uint64> reserved;
}

typedef _ExportFrameNative = Int32 Function(
  Pointer<_ErikaFrameExportOptions>,
  Pointer<_ErikaFrameExportResult>,
);
typedef _ExportFrameDart = int Function(
  Pointer<_ErikaFrameExportOptions>,
  Pointer<_ErikaFrameExportResult>,
);
typedef _FrameResultFreeNative = Void Function(Pointer<_ErikaFrameExportResult>);
typedef _FrameResultFreeDart = void Function(Pointer<_ErikaFrameExportResult>);
typedef _LastErrorNative = Pointer<Utf8> Function();
typedef _LastErrorDart = Pointer<Utf8> Function();
typedef _StringFreeNative = Void Function(Pointer<Utf8>);
typedef _StringFreeDart = void Function(Pointer<Utf8>);

Future<Map<String, Object?>> exportGifNative(Map<String, Object?> options) =>
    Isolate.run(() => _exportGifSync(options));

Map<String, Object?> _exportGifSync(Map<String, Object?> arguments) {
  final library = _openErikaLibrary();
  final exportGif = library.lookupFunction<_ExportGifNative, _ExportGifDart>(
    'erika_export_gif',
  );
  final lastError = library.lookupFunction<_LastErrorNative, _LastErrorDart>(
    'erika_last_error_message',
  );
  final stringFree = library.lookupFunction<_StringFreeNative, _StringFreeDart>(
    'erika_string_free',
  );

  final inputUri = (arguments['inputUri'] as String).toNativeUtf8();
  final outputPath = (arguments['outputPath'] as String).toNativeUtf8();
  final headerMap = Map<String, String>.from(
    arguments['httpHeaders'] as Map? ?? const <String, String>{},
  );
  final headers = headerMap.isEmpty
      ? nullptr.cast<_ErikaHttpHeader>()
      : calloc<_ErikaHttpHeader>(headerMap.length);
  final headerStrings = <Pointer<Utf8>>[];
  final options = calloc<_ErikaGifExportOptions>();
  final result = calloc<_ErikaGifExportResult>();
  try {
    var index = 0;
    for (final entry in headerMap.entries) {
      final name = entry.key.toNativeUtf8();
      final value = entry.value.toNativeUtf8();
      headerStrings.addAll(<Pointer<Utf8>>[name, value]);
      headers[index]
        ..name = name
        ..value = value;
      index += 1;
    }
    options.ref
      ..inputUri = inputUri
      ..outputPath = outputPath
      ..startMillis = arguments['startMillis'] as int
      ..endMillis = arguments['endMillis'] as int
      ..framesPerSecond = arguments['framesPerSecond'] as int
      ..outputWidth = arguments['outputWidth'] as int
      ..outputHeight = arguments['outputHeight'] as int
      ..quality = arguments['quality'] as int
      ..loopCount = arguments['loopCount'] as int
      ..overwrite = arguments['overwrite'] as bool
      ..headers = headers
      ..headerCount = headerMap.length
      ..httpReadAheadBytes = arguments['httpReadAheadBytes'] as int? ?? 0;

    final status = exportGif(options, result);
    if (status != 0) {
      final messagePointer = lastError();
      final message = messagePointer == nullptr
          ? 'ErikaStatus $status'
          : messagePointer.toDartString();
      if (messagePointer != nullptr) {
        stringFree(messagePointer);
      }
      throw StateError('GIF export failed: $message');
    }
    return <String, Object?>{
      'outputPath': arguments['outputPath'] as String,
      'width': result.ref.width,
      'height': result.ref.height,
      'frameCount': result.ref.frameCount,
      'fileSize': result.ref.fileSize,
    };
  } finally {
    calloc.free(inputUri);
    calloc.free(outputPath);
    for (final value in headerStrings) {
      calloc.free(value);
    }
    if (headers != nullptr) {
      calloc.free(headers);
    }
    calloc.free(options);
    calloc.free(result);
  }
}

Future<Map<String, Object?>> exportFrameNative(Map<String, Object?> options) =>
    Isolate.run(() => _exportFrameSync(options));

Map<String, Object?> _exportFrameSync(Map<String, Object?> arguments) {
  final library = _openErikaLibrary();
  final _ExportFrameDart exportFrame;
  final _FrameResultFreeDart freeResult;
  try {
    exportFrame = library.lookupFunction<_ExportFrameNative, _ExportFrameDart>(
      'erika_export_frame',
    );
    freeResult =
        library.lookupFunction<_FrameResultFreeNative, _FrameResultFreeDart>(
      'erika_frame_export_result_free',
    );
  } on ArgumentError {
    throw UnsupportedError(
      'The bundled liberika native library has no erika_export_frame.',
    );
  }
  final lastError = library.lookupFunction<_LastErrorNative, _LastErrorDart>(
    'erika_last_error_message',
  );
  final stringFree = library.lookupFunction<_StringFreeNative, _StringFreeDart>(
    'erika_string_free',
  );

  final inputUri = (arguments['uri'] as String).toNativeUtf8();
  final headerMap = Map<String, String>.from(
    arguments['httpHeaders'] as Map? ?? const <String, String>{},
  );
  final headers = headerMap.isEmpty
      ? nullptr.cast<_ErikaHttpHeader>()
      : calloc<_ErikaHttpHeader>(headerMap.length);
  final headerStrings = <Pointer<Utf8>>[];
  final options = calloc<_ErikaFrameExportOptions>();
  final result = calloc<_ErikaFrameExportResult>();
  try {
    var index = 0;
    for (final entry in headerMap.entries) {
      final name = entry.key.toNativeUtf8();
      final value = entry.value.toNativeUtf8();
      headerStrings.addAll(<Pointer<Utf8>>[name, value]);
      headers[index]
        ..name = name
        ..value = value;
      index += 1;
    }
    options.ref
      ..inputUri = inputUri
      ..headers = headers
      ..headerCount = headerMap.length
      ..positionMillis = arguments['positionMillis'] as int
      ..httpReadAheadBytes = arguments['httpReadAheadBytes'] as int? ?? 0
      ..maxWidth = arguments['width'] as int? ?? 0
      ..maxHeight = arguments['height'] as int? ?? 0
      ..format = arguments['format'] as int? ?? 1;

    final status = exportFrame(options, result);
    if (status != 0) {
      final messagePointer = lastError();
      final message = messagePointer == nullptr
          ? 'ErikaStatus $status'
          : messagePointer.toDartString();
      if (messagePointer != nullptr) {
        stringFree(messagePointer);
      }
      throw StateError('Frame export failed: $message');
    }
    try {
      final frame = result.ref;
      return <String, Object?>{
        'width': frame.width,
        'height': frame.height,
        'format': frame.format,
        'bytes': Uint8List.fromList(frame.data.asTypedList(frame.dataLen)),
        'timestampMicros': frame.timestampMicros,
        'durationMicros': frame.durationMicros,
        'sourceWidth': frame.sourceWidth,
        'sourceHeight': frame.sourceHeight,
      };
    } finally {
      freeResult(result);
    }
  } finally {
    calloc.free(inputUri);
    for (final value in headerStrings) {
      calloc.free(value);
    }
    if (headers != nullptr) {
      calloc.free(headers);
    }
    calloc.free(options);
    calloc.free(result);
  }
}

DynamicLibrary _openErikaLibrary() {
  if (Platform.isIOS || Platform.isMacOS) {
    return DynamicLibrary.process();
  }
  if (Platform.isWindows) {
    return DynamicLibrary.open('erika_capi.dll');
  }
  if (Platform.isAndroid || Platform.isLinux || Platform.operatingSystem == 'ohos') {
    return DynamicLibrary.open('liberika_capi.so');
  }
  throw UnsupportedError(
    'Erika headless export is not available on ${Platform.operatingSystem}.',
  );
}
