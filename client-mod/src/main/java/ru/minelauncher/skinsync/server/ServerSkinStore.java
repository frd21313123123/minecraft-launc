package ru.minelauncher.skinsync.server;

import net.minecraft.network.ConnectionProtocol;
import net.minecraft.network.chat.Component;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerPlayer;
import net.neoforged.neoforge.network.PacketDistributor;
import net.neoforged.neoforge.network.handling.IPayloadContext;
import net.neoforged.neoforge.network.registration.NetworkRegistry;
import net.neoforged.neoforge.server.ServerLifecycleHooks;
import ru.minelauncher.skinsync.SkinPngValidator;
import ru.minelauncher.skinsync.network.SkinDataPayload;
import ru.minelauncher.skinsync.network.SkinHelloPayload;
import ru.minelauncher.skinsync.network.SkinRemovePayload;
import ru.minelauncher.skinsync.network.SkinUploadPayload;

import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.UUID;

public final class ServerSkinStore {
    private static final long MIN_UPLOAD_INTERVAL_NANOS = 2_000_000_000L;
    private static final Map<UUID, SkinDataPayload> SKINS = new HashMap<>();
    private static final Map<UUID, Long> LAST_UPLOADS = new HashMap<>();
    private static final Set<UUID> GREETED_CLIENTS = new HashSet<>();

    private ServerSkinStore() {
    }

    public static void handleHello(SkinHelloPayload payload, IPayloadContext context) {
        if (context.player() instanceof ServerPlayer player
                && GREETED_CLIENTS.add(player.getUUID())) {
            sendAllSkinsTo(player);
        }
    }

    public static void handleUpload(SkinUploadPayload payload, IPayloadContext context) {
        if (!(context.player() instanceof ServerPlayer sender)) {
            return;
        }

        UUID playerId = sender.getUUID();
        long now = System.nanoTime();
        long previousUpload = LAST_UPLOADS.getOrDefault(playerId, 0L);
        if (now - previousUpload < MIN_UPLOAD_INTERVAL_NANOS) {
            return;
        }
        LAST_UPLOADS.put(playerId, now);

        String validationError = SkinPngValidator.validate(payload.png());
        if (validationError != null) {
            sender.sendSystemMessage(Component.literal(
                    "[MineLauncher] Скин отклонён сервером: " + validationError
            ));
            return;
        }

        SkinDataPayload previous = SKINS.get(playerId);
        if (previous != null
                && previous.slim() == payload.slim()
                && Arrays.equals(previous.png(), payload.png())) {
            sendAllSkinsTo(sender);
            return;
        }

        SkinDataPayload skin = new SkinDataPayload(
                playerId,
                Arrays.copyOf(payload.png(), payload.png().length),
                payload.slim()
        );
        SKINS.put(playerId, skin);

        sendAllSkinsTo(sender);
        for (ServerPlayer player : sender.server.getPlayerList().getPlayers()) {
            if (player != sender && supports(player, SkinDataPayload.TYPE.id())) {
                PacketDistributor.sendToPlayer(player, skin);
            }
        }
    }

    public static void remove(UUID playerId) {
        GREETED_CLIENTS.remove(playerId);
        if (SKINS.remove(playerId) == null) {
            LAST_UPLOADS.remove(playerId);
            return;
        }
        LAST_UPLOADS.remove(playerId);
        SkinRemovePayload payload = new SkinRemovePayload(playerId);
        MinecraftServer server = ServerLifecycleHooks.getCurrentServer();
        if (server == null) {
            return;
        }
        server.getPlayerList()
                .getPlayers()
                .stream()
                .filter(player -> supports(player, SkinRemovePayload.TYPE.id()))
                .forEach(player -> PacketDistributor.sendToPlayer(player, payload));
    }

    public static void clear() {
        SKINS.clear();
        LAST_UPLOADS.clear();
        GREETED_CLIENTS.clear();
    }

    private static void sendAllSkinsTo(ServerPlayer player) {
        if (!supports(player, SkinDataPayload.TYPE.id())) {
            return;
        }
        for (SkinDataPayload skin : SKINS.values()) {
            PacketDistributor.sendToPlayer(player, skin);
        }
    }

    private static boolean supports(
            ServerPlayer player,
            net.minecraft.resources.ResourceLocation channel
    ) {
        return NetworkRegistry.hasChannel(
                player.connection.getConnection(),
                ConnectionProtocol.PLAY,
                channel
        );
    }
}
