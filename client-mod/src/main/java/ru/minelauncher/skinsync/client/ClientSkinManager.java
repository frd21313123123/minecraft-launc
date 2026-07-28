package ru.minelauncher.skinsync.client;

import com.mojang.blaze3d.platform.NativeImage;
import net.minecraft.client.Minecraft;
import net.minecraft.client.renderer.texture.DynamicTexture;
import net.minecraft.client.resources.PlayerSkin;
import net.minecraft.resources.ResourceLocation;
import ru.minelauncher.skinsync.MineLauncherSkinSync;
import ru.minelauncher.skinsync.network.SkinDataPayload;
import ru.minelauncher.skinsync.network.SkinRemovePayload;

import java.io.IOException;
import java.util.HashMap;
import java.util.Map;
import java.util.UUID;

public final class ClientSkinManager {
    private static final Map<UUID, PlayerSkin> SKINS = new HashMap<>();

    private ClientSkinManager() {
    }

    public static PlayerSkin get(UUID playerId) {
        return SKINS.get(playerId);
    }

    public static void apply(SkinDataPayload payload) {
        NativeImage image;
        try {
            image = NativeImage.read(payload.png());
        } catch (IOException error) {
            return;
        }
        if (image.getWidth() != 64 || image.getHeight() != 64) {
            image.close();
            return;
        }

        ResourceLocation texture = textureLocation(payload.playerId());
        if (SKINS.containsKey(payload.playerId())) {
            Minecraft.getInstance().getTextureManager().release(texture);
        }
        Minecraft.getInstance().getTextureManager().register(
                texture,
                new DynamicTexture(image)
        );
        PlayerSkin.Model model = payload.slim()
                ? PlayerSkin.Model.SLIM
                : PlayerSkin.Model.WIDE;
        SKINS.put(
                payload.playerId(),
                new PlayerSkin(texture, null, null, null, model, false)
        );
    }

    public static void remove(SkinRemovePayload payload) {
        remove(payload.playerId());
    }

    public static void remove(UUID playerId) {
        if (SKINS.remove(playerId) != null) {
            Minecraft.getInstance().getTextureManager().release(textureLocation(playerId));
        }
    }

    public static void clear() {
        for (UUID playerId : SKINS.keySet()) {
            Minecraft.getInstance().getTextureManager().release(textureLocation(playerId));
        }
        SKINS.clear();
    }

    private static ResourceLocation textureLocation(UUID playerId) {
        return ResourceLocation.fromNamespaceAndPath(
                MineLauncherSkinSync.MOD_ID,
                "player/" + playerId.toString().replace("-", "")
        );
    }
}
