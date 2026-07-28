package ru.minelauncher.skinsync;

import javax.imageio.ImageIO;
import java.awt.image.BufferedImage;
import java.io.ByteArrayInputStream;
import java.io.IOException;

public final class SkinPngValidator {
    public static final int MAX_SKIN_BYTES = 256 * 1024;
    private static final byte[] PNG_SIGNATURE = {
            (byte) 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A
    };

    private SkinPngValidator() {
    }

    public static String validate(byte[] png) {
        if (png == null || png.length < 33) {
            return "PNG-файл пуст или повреждён";
        }
        if (png.length > MAX_SKIN_BYTES) {
            return "PNG-скин превышает 256 КБ";
        }
        for (int index = 0; index < PNG_SIGNATURE.length; index++) {
            if (png[index] != PNG_SIGNATURE[index]) {
                return "Файл не является PNG";
            }
        }
        if (readInt(png, 8) != 13
                || png[12] != 'I'
                || png[13] != 'H'
                || png[14] != 'D'
                || png[15] != 'R') {
            return "У PNG отсутствует корректный заголовок IHDR";
        }

        int width = readInt(png, 16);
        int height = readInt(png, 20);
        if (width != 64 || height != 64) {
            return "Нужен современный PNG-скин размером 64×64";
        }
        int bitDepth = png[24] & 0xFF;
        int colorType = png[25] & 0xFF;
        if (bitDepth != 8 || (colorType != 2 && colorType != 6)) {
            return "PNG должен использовать 8-битный RGB или RGBA";
        }
        if (png[26] != 0 || png[27] != 0) {
            return "PNG использует неподдерживаемое сжатие или фильтр";
        }
        try {
            BufferedImage image = ImageIO.read(new ByteArrayInputStream(png));
            if (image == null || image.getWidth() != 64 || image.getHeight() != 64) {
                return "Не удалось декодировать PNG-скин 64×64";
            }
        } catch (IOException error) {
            return "PNG-файл повреждён";
        }
        return null;
    }

    private static int readInt(byte[] bytes, int offset) {
        return ((bytes[offset] & 0xFF) << 24)
                | ((bytes[offset + 1] & 0xFF) << 16)
                | ((bytes[offset + 2] & 0xFF) << 8)
                | (bytes[offset + 3] & 0xFF);
    }
}
