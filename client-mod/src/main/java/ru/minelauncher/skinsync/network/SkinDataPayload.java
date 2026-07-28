package ru.minelauncher.skinsync.network;

import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import ru.minelauncher.skinsync.MineLauncherSkinSync;
import ru.minelauncher.skinsync.SkinPngValidator;

import java.util.UUID;

public record SkinDataPayload(UUID playerId, byte[] png, boolean slim)
        implements CustomPacketPayload {
    public static final Type<SkinDataPayload> TYPE = new Type<>(
            ResourceLocation.fromNamespaceAndPath(MineLauncherSkinSync.MOD_ID, "skin")
    );
    public static final StreamCodec<RegistryFriendlyByteBuf, SkinDataPayload> STREAM_CODEC =
            StreamCodec.of(
                    (buffer, payload) -> {
                        buffer.writeUUID(payload.playerId());
                        buffer.writeByteArray(payload.png());
                        buffer.writeBoolean(payload.slim());
                    },
                    buffer -> new SkinDataPayload(
                            buffer.readUUID(),
                            buffer.readByteArray(SkinPngValidator.MAX_SKIN_BYTES),
                            buffer.readBoolean()
                    )
            );

    @Override
    public Type<? extends CustomPacketPayload> type() {
        return TYPE;
    }
}
