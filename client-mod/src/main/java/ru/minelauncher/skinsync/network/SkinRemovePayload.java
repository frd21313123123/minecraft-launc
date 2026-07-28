package ru.minelauncher.skinsync.network;

import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import ru.minelauncher.skinsync.MineLauncherSkinSync;

import java.util.UUID;

public record SkinRemovePayload(UUID playerId) implements CustomPacketPayload {
    public static final Type<SkinRemovePayload> TYPE = new Type<>(
            ResourceLocation.fromNamespaceAndPath(MineLauncherSkinSync.MOD_ID, "remove")
    );
    public static final StreamCodec<RegistryFriendlyByteBuf, SkinRemovePayload> STREAM_CODEC =
            StreamCodec.of(
                    (buffer, payload) -> buffer.writeUUID(payload.playerId()),
                    buffer -> new SkinRemovePayload(buffer.readUUID())
            );

    @Override
    public Type<? extends CustomPacketPayload> type() {
        return TYPE;
    }
}
