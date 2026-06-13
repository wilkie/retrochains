int main(void) {
  char buf[3];
  buf[0] = 0x30;
  buf[1] = 0x05;
  return buf[0] | buf[1];
}
