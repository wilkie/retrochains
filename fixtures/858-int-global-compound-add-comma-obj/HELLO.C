int g;
int main() {
  int y;
  int z;
  g = 100;
  y = 5;
  g += (y = 3, z = y + 1, z);
  return 0;
}
