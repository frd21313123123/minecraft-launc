package ru.minelauncher.skinsync.client;

import com.google.gson.Gson;
import com.google.gson.JsonSyntaxException;
import net.minecraft.client.Minecraft;
import net.minecraft.network.ConnectionProtocol;
import net.minecraft.network.chat.Component;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.client.event.ClientPlayerNetworkEvent;
import net.neoforged.neoforge.client.event.ClientTickEvent;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.network.PacketDistributor;
import net.neoforged.neoforge.network.registration.NetworkRegistry;
import ru.minelauncher.skinsync.MineLauncherSkinSync;
import ru.minelauncher.skinsync.SkinPngValidator;
import ru.minelauncher.skinsync.network.ClientPayloadBridge;
import ru.minelauncher.skinsync.network.SkinHelloPayload;
import ru.minelauncher.skinsync.network.SkinUploadPayload;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

@Mod(value = MineLauncherSkinSync.MOD_ID, dist = Dist.CLIENT)
public final class MineLauncherSkinSyncClient {
    private static final String REQUEST_FILE = "minelauncher-skin-sync.json";
    private static final String SKIN_FILE = "minelauncher-skin.png";
    private static final int SEND_DELAY_TICKS = 20;
    private static final Gson GSON = new Gson();

    private int ticksUntilSend = -1;
    private boolean sentForConnection;

    public MineLauncherSkinSyncClient() {
        ClientPayloadBridge.install(ClientSkinManager::apply, ClientSkinManager::remove);
        NeoForge.EVENT_BUS.addListener(this::onLoggingIn);
        NeoForge.EVENT_BUS.addListener(this::onLoggingOut);
        NeoForge.EVENT_BUS.addListener(this::onClientTick);
    }

    private void onLoggingIn(ClientPlayerNetworkEvent.LoggingIn event) {
        ClientSkinManager.clear();
        sentForConnection = false;
        ticksUntilSend = SEND_DELAY_TICKS;
    }

    private void onLoggingOut(ClientPlayerNetworkEvent.LoggingOut event) {
        ticksUntilSend = -1;
        sentForConnection = false;
        ClientSkinManager.clear();
    }

    private void onClientTick(ClientTickEvent.Post event) {
        if (sentForConnection || ticksUntilSend < 0) {
            return;
        }
        if (ticksUntilSend-- > 0) {
            return;
        }
        sentForConnection = true;
        sendConfiguredSkin();
    }

    private static void sendConfiguredSkin() {
        Minecraft minecraft = Minecraft.getInstance();
        if (minecraft.player == null || minecraft.getConnection() == null) {
            return;
        }

        Path gameDirectory = minecraft.gameDirectory.toPath();
        SyncRequest request = readRequest(gameDirectory.resolve(REQUEST_FILE));
        boolean wantsCustomSkin =
                request != null && request.version == 2 && request.enabled;
        if (!NetworkRegistry.hasChannel(
                minecraft.getConnection().getConnection(),
                ConnectionProtocol.PLAY,
                SkinHelloPayload.TYPE.id()
        )) {
            if (wantsCustomSkin) {
                showMessage(
                        "Сервер не поддерживает MineLauncher Skin Sync. "
                                + "Установите мод на сервер."
                );
            }
            return;
        }
        PacketDistributor.sendToServer(new SkinHelloPayload());
        if (!wantsCustomSkin) {
            return;
        }
        if (request.username == null
                || !minecraft.player.getGameProfile().getName()
                .equalsIgnoreCase(request.username.trim())) {
            showMessage("Скин не применён: профиль лаунчера не совпадает с игровым.");
            return;
        }

        byte[] png;
        try {
            png = Files.readAllBytes(gameDirectory.resolve(SKIN_FILE));
        } catch (IOException error) {
            showMessage("Не удалось прочитать PNG, подготовленный лаунчером.");
            return;
        }
        String validationError = SkinPngValidator.validate(png);
        if (validationError != null) {
            showMessage("Скин не применён: " + validationError);
            return;
        }

        boolean slim = "slim".equalsIgnoreCase(request.model);
        PacketDistributor.sendToServer(new SkinUploadPayload(png, slim));
        showMessage("Скин отправлен серверу.");
    }

    private static SyncRequest readRequest(Path path) {
        try {
            String json = Files.readString(path, StandardCharsets.UTF_8);
            return GSON.fromJson(json, SyncRequest.class);
        } catch (IOException | JsonSyntaxException ignored) {
            return null;
        }
    }

    private static void showMessage(String message) {
        Minecraft minecraft = Minecraft.getInstance();
        if (minecraft.player != null) {
            minecraft.player.displayClientMessage(
                    Component.literal("[MineLauncher] " + message),
                    false
            );
        }
    }

    private static final class SyncRequest {
        int version;
        boolean enabled;
        String username;
        String model;
    }
}
