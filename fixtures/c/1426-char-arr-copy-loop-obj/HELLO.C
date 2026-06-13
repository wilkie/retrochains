char src[3] = "ab";
char dst[3];
int main(void) {
  int i;
  for (i = 0; i < 3; i++) dst[i] = src[i];
  return dst[1];
}
