int main(void) {
  int x;
  x = 0;
  goto mid;
top:
mid:
end:
  x = x + 1;
  return x;
}
