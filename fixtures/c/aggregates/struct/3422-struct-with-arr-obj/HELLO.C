struct Box {
  int header;
  int data[3];
} b;

int get(int i) {
  return b.data[i];
}
