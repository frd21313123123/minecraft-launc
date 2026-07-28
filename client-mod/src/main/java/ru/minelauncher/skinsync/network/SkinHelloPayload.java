package ru.minelauncher.skinsync.network;

import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.ResourceLocation;
import ru.minelauncher.skinsync.MineLauncherSkinSync;

public record SkinHelloPayload() implements CustomPacketPayload {
    public static final Type<SkinHelloPayload> TYPE = new Type<>(
            ResourceLocation.fromNamespaceAndPath(MineLauncherSkinSync.MOD_ID, "hello")
    );
    public static final StreamCodec<RegistryFriendlyByteBuf, SkinHelloPayload> STREAM_CODEC =
            StreamCodec.unit(new SkinHelloPayload());

    @Override
    public Type<? extends CustomPacketPayload> type() {
        return TYPE;
    }
}
