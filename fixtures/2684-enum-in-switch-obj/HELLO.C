enum Color { RED, GREEN, BLUE };
int hex(enum Color c) {
  switch (c) {
    case RED:   return 0xF00;
    case GREEN: return 0x0F0;
    case BLUE:  return 0x00F;
  }
  return 0;
}
