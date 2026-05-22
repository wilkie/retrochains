union IntBytes {
  unsigned int i;
  unsigned char b[2];
};
int main(void) {
  union IntBytes u;
  u.i = 0xABCD;
  return u.b[0] + u.b[1];
}
