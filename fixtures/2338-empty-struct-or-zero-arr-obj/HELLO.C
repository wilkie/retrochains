struct Header {
  int magic;
  int size;
  char data[1];
};
int main(void) {
  static struct Header h = {0xCAFE, 5, {'A'}};
  return h.magic + h.size + h.data[0];
}
