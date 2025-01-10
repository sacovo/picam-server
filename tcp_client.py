import socket
import struct
import cv2
import av
import sys
import pygame

import io

screen_width = 1920
screen_height = 1080

def load_image_from_bytes(image_bytes):
    image = pygame.image.load(io.BytesIO(image_bytes))
    return pygame.transform.scale(image, (screen_width, screen_height))

def main():
    pygame.init()
    if len(sys.argv) != 3:
        print("Usage: python tcp_client.py <host> <port>")
        sys.exit(1)

    font_size = 36
    font = pygame.font.Font(None, font_size)  # Use the default pygame font; set to None to specify a font file
    font_color = (255, 255, 255)  # White color for text
    screen = pygame.display.set_mode((screen_width, screen_height))
    pygame.display.set_caption("Stream")

    host = sys.argv[1]
    port = int(sys.argv[2])

    decoder = av.CodecContext.create("h264", "r")
    i = 0
    running = True
    clock = pygame.time.Clock()

    try:
        # Create a socket object
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as client_socket:
            # Connect to the server
            client_socket.connect((host, port))
            # print(f"Connected to {host}:{port}")
            next_frame = struct.pack("<h", 10)

            # Receive and print data
            while True:
                for event in pygame.event.get():
                    if event.type == pygame.QUIT:
                        pygame.quit()
                        return
                client_socket.sendall(next_frame)
                if i == 10:
                    out = struct.pack("<hii", 1, 1280, 720)
                    client_socket.sendall(out)
                    # print("Sent data!")
                # if i == 20:
                #     out = struct.pack("<hi", 2, 40_000)
                #     client_socket.sendall(out)
                #     # print("Sent data!")
                # if i == 30:
                #     out = struct.pack("<hi", 3, 4)
                #     client_socket.sendall(out)
                #     # print("Sent data!")
                # if i == 30:
                #     out = struct.pack("<hi", 4, 3)
                #     client_socket.sendall(out)
                #     # print("Sent data!")

                b = client_socket.recv(4)
                n_bytes = int.from_bytes(b, byteorder='big', signed=False)
                # print(f"Reading {n_bytes}")
                buff = bytearray(n_bytes)

                read = 0
                while read < n_bytes:
                    cr = client_socket.recv_into(memoryview(buff)[read:])
                    if cr == 0:
                        raise EOFError
                    read += cr

                packets = decoder.parse(buff)
                for packet in packets:
                    try:
                        for frame in decoder.decode(packet):
                            img = frame.to_ndarray(format="bgr24")
                            img = cv2.imencode('.jpg', img)[1].tobytes()
                            img = load_image_from_bytes(img)
                            screen.blit(img, (0, 0))
                            text = f"Packet size: {n_bytes}"
                            tf = font.render(text, True, font_color, (0, 0, 0))
                            screen.blit(tf, (10, 10))
                            pygame.display.flip()
                            clock.tick(5)
                            i += 1
                    except av.InvalidDataError:
                        continue


    except ConnectionError as e:
        print(f"Connection error: {e}")
    except KeyboardInterrupt:
        print("\nConnection closed.")
        sys.exit(0)

if __name__ == "__main__":
    main()
