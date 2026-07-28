package ru.minelauncher.skinsync.network;

import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import ru.minelauncher.skinsync.MineLauncherSkinSync;
import ru.minelauncher.skinsync.SkinPngValidator;

public record SkinUploadPayload(byte[] png, boolean slim) implements CustomPacketPayload {
    public static final Type<SkinUploadPayload> TYPE = new Type<>(
            ResourceLocation.fromNamespaceAndPath(MineLauncherSkinSync.MOD_ID, "upload")
    );
    public static final StreamCodec<RegistryFriendlyByteBuf, SkinUploadPayload> STREAM_CODEC =
            StreamCodec.of(
                    (buffer, payload) -> {
                        buffer.writeByteArray(payload.png());
                        buffer.writeBoolean(payload.slim());
                    },
                    buffer -> new SkinUploadPayload(
                            buffer.readByteArray(SkinPngValidator.MAX_SKIN_BYTES),
                            buffer.readBoolean()
                    )
            );

    @Override
    public Type<? extends CustomPacketPayload> type() {
        return TYPE;
    }
}
